use std::fmt;
use std::io;

use super::session::SideFile;

/// The unit a storage device writes whole, and the most a torn write keeps or loses together.
pub(crate) const SECTOR_SIZE: usize = 512;

const LARGEST_BLOCK: usize = 1 << 30;

/// The fixed size of every block of one file, a power of two from one sector up to 1 GiB.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BlockSize(usize);

impl BlockSize {
    pub(crate) fn new(bytes: usize) -> Option<Self> {
        (bytes.is_power_of_two() && (SECTOR_SIZE..=LARGEST_BLOCK).contains(&bytes))
            .then_some(Self(bytes))
    }

    pub(crate) fn bytes(self) -> usize {
        self.0
    }

    /// The byte offset where block `block` starts.
    pub(crate) fn offset(self, block: u64) -> io::Result<u64> {
        block
            .checked_mul(self.0 as u64)
            .ok_or_else(|| invalid("block number out of range"))
    }

    /// Fails unless a buffer of `length` bytes holds whole blocks.
    pub(crate) fn require_whole_blocks(self, length: usize) -> io::Result<()> {
        if length.is_multiple_of(self.0) {
            Ok(())
        } else {
            Err(invalid("a buffer must hold whole blocks"))
        }
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

/// A file read and written in whole blocks of one fixed size, addressed by block number.
pub(crate) trait BlockFile: Send + Sync + fmt::Debug {
    /// Fills `buffer`, which holds whole blocks, with the blocks from `first` on, and fails
    /// when one of them lies past the end.
    fn read(&self, first: u64, buffer: &mut [u8]) -> io::Result<()>;

    /// Writes `buffer`, which holds whole blocks, from block `first` on. A write past the end
    /// extends the file, and the blocks it skips read as zeros.
    fn write(&self, first: u64, buffer: &[u8]) -> io::Result<()>;

    /// The number of whole blocks in the file.
    fn block_count(&self) -> io::Result<u64>;

    /// Whether the file holds no byte at all, not even part of a block.
    fn is_empty(&self) -> io::Result<bool>;

    /// Takes an exclusive lock on the file until this handle closes, and fails while another
    /// handle holds a lock of either kind.
    fn lock(&self) -> io::Result<()>;

    /// Takes a shared lock on the file until this handle closes, and fails while another handle
    /// holds an exclusive one. Any number of handles hold shared locks at once.
    fn lock_shared(&self) -> io::Result<()>;

    /// Takes the shared lock that every process of a multi-process session holds on the
    /// database file, which keeps out exclusive locks of other handles but no reader or writer,
    /// and gives up any exclusive lock this handle holds, with no moment at which it holds
    /// neither.
    fn lock_session(&self) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "this file takes part in no multi-process session",
        ))
    }

    /// Gives up the exclusive lock this handle holds.
    fn unlock(&self) -> io::Result<()> {
        Ok(())
    }

    /// Truncates the file to `count` blocks, or extends it with zeroed blocks.
    fn set_block_count(&self, count: u64) -> io::Result<()>;

    /// Returns once the file's blocks and block count survive a power loss.
    fn sync(&self) -> io::Result<()>;
}

/// A flat set of named block files, such as database files and WAL segments.
pub(crate) trait FileSystem: Send + Sync {
    type File: BlockFile + 'static;

    /// Creates an empty file of blocks of `size`, and fails when `name` exists.
    fn create(&self, name: &str, size: BlockSize) -> io::Result<Self::File>;

    /// Opens an existing file as blocks of `size`, and fails with [`io::ErrorKind::NotFound`]
    /// when there is none.
    fn open(&self, name: &str, size: BlockSize) -> io::Result<Self::File>;

    /// Renames `from` to `to`, replacing a file named `to`.
    fn rename(&self, from: &str, to: &str) -> io::Result<()>;

    fn remove(&self, name: &str) -> io::Result<()>;

    /// The names of every file, in ascending order.
    fn names(&self) -> io::Result<Vec<String>>;

    /// Returns once every create, rename, and remove so far survives a power loss.
    fn sync_names(&self) -> io::Result<()>;

    /// Opens the existing side file `name` of a multi-process session for reading and writing,
    /// and fails with [`io::ErrorKind::NotFound`] when there is none, or with
    /// [`io::ErrorKind::Unsupported`] where processes cannot share it.
    fn open_side(&self, name: &str) -> io::Result<Box<dyn SideFile>> {
        Err(no_sessions(name))
    }

    /// Creates the side file `name`, and fails with [`io::ErrorKind::AlreadyExists`] when it
    /// exists.
    fn create_side(&self, name: &str) -> io::Result<Box<dyn SideFile>> {
        Err(no_sessions(name))
    }
}

fn no_sessions(name: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        format!("no multi-process session can keep {name} here"),
    )
}

#[cfg(test)]
#[path = "block_file.test.rs"]
mod tests;
