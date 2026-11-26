use std::error::Error;
use std::fmt;
use std::path::Path;

use crate::error::ErrorKind;

pub(crate) mod backup;
#[cfg(test)]
pub(crate) mod conformance;
#[cfg(test)]
pub(crate) mod model;
#[cfg(test)]
pub(crate) mod test_engine;

macro_rules! tables {
    ($($(#[$meta:meta])* $variant:ident,)*) => {
        #[doc = "The fixed set of ordered key-value tables the graph layer is built on."]
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub(crate) enum Table {
            $($(#[$meta])* $variant,)*
        }

        impl Table {
            pub(crate) const ALL: [Table; 0 $(+ { stringify!($variant); 1 })*] =
                [$(Table::$variant),*];
        }
    };
}

tables! {
    Metadata,
    TokenByName,
    TokenById,
    #[doc = "The node records of format versions before property columns, which only their upgrade reads."]
    Nodes,
    #[doc = "The relationship records of format versions before relationship chunks, one entry per relationship, which only their upgrade reads."]
    Relationships,
    #[doc = "The outgoing adjacency of format versions before adjacency segments, which only their upgrade reads."]
    Outgoing,
    #[doc = "The incoming adjacency of format versions before adjacency segments, which only their upgrade reads."]
    Incoming,
    #[doc = "The label index of format versions before node meta, which only their upgrade reads."]
    LabelIndex,
    Indexes,
    KeywordPostings,
    KeywordDocuments,
    KeywordTotals,
    Vectors,
    PropertyEntries,
    NodeChunks,
    LabelSets,
    LabelSetByTokens,
    NodeColumns,
    Adjacency,
    RelationshipChunks,
    #[doc = "The terms of the writers a database has followed or been, with the LSN each began at, which only replication writes."]
    ReplicationHistory,
}

impl Table {
    /// Whether only the upgrade of an older format version reads the table, so a new database
    /// does not create it.
    pub(crate) const fn is_legacy(self) -> bool {
        matches!(
            self,
            Table::LabelIndex
                | Table::Nodes
                | Table::Relationships
                | Table::Outgoing
                | Table::Incoming
        )
    }
}

pub(crate) type Entry = (Vec<u8>, Vec<u8>);

/// A failure reported by the storage engine, kept opaque so no engine type leaks upward.
#[derive(Debug)]
pub(crate) struct StorageError {
    source: Box<dyn Error + Send + Sync>,
    kind: ErrorKind,
}

impl StorageError {
    pub(crate) fn new(source: impl Error + Send + Sync + 'static) -> Self {
        Self::of_kind(ErrorKind::Storage, source)
    }

    /// A failure that hosts tell apart from other storage failures by `kind`.
    pub(crate) fn of_kind(kind: ErrorKind, source: impl Error + Send + Sync + 'static) -> Self {
        Self {
            source: Box::new(source),
            kind,
        }
    }

    /// A failure the engine raised on damaged stored data, which is reported as corrupt data.
    pub(crate) fn corrupt(source: impl Error + Send + Sync + 'static) -> Self {
        Self::of_kind(ErrorKind::CorruptData, source)
    }

    pub(crate) fn is_corrupt(&self) -> bool {
        self.kind == ErrorKind::CorruptData
    }

    /// The kind the public error reports for this failure.
    pub(crate) fn kind(&self) -> ErrorKind {
        self.kind
    }
}

impl fmt::Display for StorageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "storage failure: {}", self.source)
    }
}

impl Error for StorageError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.source.as_ref())
    }
}

/// An ordered key-value store over the fixed [`Table`] set with snapshot read
/// transactions and a single write transaction at a time.
pub(crate) trait Storage {
    type Read<'a>: ReadTransaction
    where
        Self: 'a;
    type Write<'a>: WriteTransaction
    where
        Self: 'a;

    fn begin_read(&self) -> Result<Self::Read<'_>, StorageError>;

    fn begin_write(&self) -> Result<Self::Write<'_>, StorageError>;

    /// Writes every table that `snapshot` sees into a new database of this backend at `path`,
    /// where no file exists, and returns once that one file holds the copy durably and whole.
    fn write_copy(snapshot: &Self::Read<'_>, path: &Path) -> Result<(), StorageError>
    where
        Self: Sized;

    /// Writes every commit so far into the database file, so a snapshot taken next holds them.
    fn checkpoint(&self) -> Result<(), StorageError> {
        Ok(())
    }
}

const COPY_BATCH_BYTES: usize = 64 << 20;

/// Copies every table but the legacy ones from `snapshot` into the empty database `target`, in
/// write transactions that each commit once they hold [`COPY_BATCH_BYTES`].
pub(crate) fn copy_tables<S: Storage>(
    snapshot: &impl ReadTransaction,
    target: &S,
) -> Result<(), StorageError> {
    let mut write: Option<S::Write<'_>> = None;
    let mut pending = 0usize;
    for table in Table::ALL.into_iter().filter(|table| !table.is_legacy()) {
        snapshot.visit_prefix(table, &[], |key, value| {
            let open = match &mut write {
                Some(open) => open,
                None => write.insert(target.begin_write()?),
            };
            open.put(table, key, value)?;
            pending = pending.saturating_add(key.len() + value.len());
            if pending >= COPY_BATCH_BYTES {
                pending = 0;
                if let Some(full) = write.take() {
                    full.commit()?;
                }
            }
            Ok::<(), StorageError>(())
        })?;
    }
    match write {
        Some(last) => last.commit(),
        None => Ok(()),
    }
}

/// A value as a scan that leaves the values stored apart from their keys unread meets it.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Peek<'a> {
    /// The value, which the store held at hand.
    Bytes(&'a [u8]),
    /// The length of a value stored apart, which [`ReadTransaction::get_with`] reads.
    Apart(usize),
}

pub(crate) trait ReadTransaction {
    fn get(&self, table: Table, key: &[u8]) -> Result<Option<Vec<u8>>, StorageError>;

    /// Every entry whose key starts with `prefix`, in ascending key order. An empty prefix scans the table.
    fn scan_prefix(&self, table: Table, prefix: &[u8]) -> Result<Vec<Entry>, StorageError>;

    /// Every entry whose key is at least `start` and below `end`, in ascending key order.
    fn scan_range(
        &self,
        table: Table,
        start: &[u8],
        end: &[u8],
    ) -> Result<Vec<Entry>, StorageError>;

    /// Calls `read` with the value stored under `key`, without copying it.
    fn get_with<R, E: From<StorageError>>(
        &self,
        table: Table,
        key: &[u8],
        read: impl FnOnce(&[u8]) -> Result<R, E>,
    ) -> Result<Option<R>, E>;

    /// Calls `visit` with every entry whose key starts with `prefix`, in ascending key order,
    /// without copying them.
    fn visit_prefix<E: From<StorageError>>(
        &self,
        table: Table,
        prefix: &[u8],
        visit: impl FnMut(&[u8], &[u8]) -> Result<(), E>,
    ) -> Result<(), E>;

    /// Calls `visit` with every entry whose key is at least `start` and below `end`, in
    /// ascending key order, without copying them. An empty or reversed range visits nothing.
    fn visit_range<E: From<StorageError>>(
        &self,
        table: Table,
        start: &[u8],
        end: &[u8],
        visit: impl FnMut(&[u8], &[u8]) -> Result<(), E>,
    ) -> Result<(), E>;

    /// [`visit_prefix`](Self::visit_prefix), giving only the length of each value the store
    /// keeps apart from its key, so a scan that needs few of those values reads none of them.
    fn visit_prefix_peek<E: From<StorageError>>(
        &self,
        table: Table,
        prefix: &[u8],
        mut visit: impl FnMut(&[u8], Peek<'_>) -> Result<(), E>,
    ) -> Result<(), E> {
        self.visit_prefix(table, prefix, |key, value| visit(key, Peek::Bytes(value)))
    }

    /// [`visit_range`](Self::visit_range), giving only the length of each value the store
    /// keeps apart from its key.
    fn visit_range_peek<E: From<StorageError>>(
        &self,
        table: Table,
        start: &[u8],
        end: &[u8],
        mut visit: impl FnMut(&[u8], Peek<'_>) -> Result<(), E>,
    ) -> Result<(), E> {
        self.visit_range(table, start, end, |key, value| {
            visit(key, Peek::Bytes(value))
        })
    }
}

/// The size of a database on disk and how its cache serves reads, as a backend reports them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct StorageStats {
    pub(crate) file_bytes: u64,
    pub(crate) live_bytes: u64,
    pub(crate) live_extents: u64,
    pub(crate) free_extents: u64,
    pub(crate) wal_bytes: u64,
    pub(crate) cache_hits: u64,
    pub(crate) cache_misses: u64,
    pub(crate) held_bytes: u64,
}

/// Upkeep of a database file that no transaction does: compaction, statistics, and a check of
/// every stored invariant.
pub(crate) trait Maintenance {
    /// Writes every commit into the file and moves the live data into as little of it as it
    /// needs, then shortens the file.
    fn compact(&self) -> Result<(), StorageError>;

    fn stats(&self) -> Result<StorageStats, StorageError>;

    /// Reads every page the file's last durable state reaches and fails as corrupt data on the
    /// first broken invariant.
    fn check(&self) -> Result<(), StorageError>;
}

/// A write transaction sees its own writes. Dropping it without commit discards them.
pub(crate) trait WriteTransaction: ReadTransaction {
    fn put(&mut self, table: Table, key: &[u8], value: &[u8]) -> Result<(), StorageError>;

    fn delete(&mut self, table: Table, key: &[u8]) -> Result<(), StorageError>;

    /// Deletes every key from `start`, inclusive, to `end`, exclusive. An empty or reversed range
    /// deletes nothing.
    fn delete_range(&mut self, table: Table, start: &[u8], end: &[u8]) -> Result<(), StorageError>;

    fn commit(self) -> Result<(), StorageError>;

    fn rollback(self) -> Result<(), StorageError>;
}
