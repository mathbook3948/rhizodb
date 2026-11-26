use std::fmt;

use crate::engine::any_fs::AnyFs;
use crate::engine::snapshot::Export;
use crate::error::Error;
use crate::replication::Position;
use crate::storage::StorageError;

/// A copy of the database as its last checkpoint left it, from [`Database::snapshot`], read as
/// a stream of bytes that [`Database::install_snapshot`] turns into a database on another node.
///
/// The stream is cut into chunks that each carry a checksum, so a damaged or misplaced chunk
/// fails the install instead of becoming data. [`Snapshot::read_at`] returns the same bytes for
/// an offset every time, so a transfer cut off part way resumes from the offset it reached.
///
/// While a `Snapshot` lives, the database keeps every page of its checkpoint and the log after
/// its [position](Snapshot::position), so writes, deletes, and compaction go on without
/// changing the copy. Drop it once the transfer ends, after acknowledging the receiver's
/// position with [`Database::acknowledge`] so the log after it stays for the receiver.
///
/// [`Database::snapshot`]: crate::Database::snapshot
/// [`Database::install_snapshot`]: crate::Database::install_snapshot
/// [`Database::acknowledge`]: crate::Database::acknowledge
pub struct Snapshot<'db> {
    export: Export<'db, AnyFs>,
}

impl<'db> Snapshot<'db> {
    pub(crate) fn new(export: Export<'db, AnyFs>) -> Self {
        Self { export }
    }

    /// The position of the newest change the snapshot holds. A database installed from it
    /// holds every change up to this one and follows its writer's changes from the next.
    pub fn position(&self) -> Position {
        Position::of(self.export.epoch().clone(), self.export.position())
    }

    /// The bytes of the whole stream.
    #[expect(
        clippy::len_without_is_empty,
        reason = "a snapshot stream is never empty"
    )]
    pub fn len(&self) -> u64 {
        self.export.len()
    }

    /// Copies the stream's bytes from `offset` on into `buffer` and returns how many it copied,
    /// which is less than the buffer only at the end of the stream, and zero at or past it.
    ///
    /// Fails with [`ErrorKind::Storage`](crate::ErrorKind::Storage) when reading the database
    /// file fails.
    pub fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<usize, Error> {
        Ok(self
            .export
            .read_at(offset, buffer)
            .map_err(StorageError::from)?)
    }
}

impl fmt::Debug for Snapshot<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Snapshot")
            .field("position", &self.position())
            .field("len", &self.len())
            .finish()
    }
}
