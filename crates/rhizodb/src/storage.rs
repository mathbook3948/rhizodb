use std::error::Error;
use std::fmt;

#[cfg(test)]
pub(crate) mod conformance;
#[cfg(test)]
pub(crate) mod model;
pub(crate) mod redb_backend;

/// The fixed set of ordered key-value tables the graph layer is built on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum Table {
    Metadata,
    TokenByName,
    TokenById,
    Nodes,
    Relationships,
    Outgoing,
    Incoming,
    LabelIndex,
    Indexes,
    KeywordPostings,
    KeywordDocuments,
    KeywordTotals,
    Vectors,
    PropertyEntries,
}

impl Table {
    pub(crate) const ALL: [Table; 14] = [
        Table::Metadata,
        Table::TokenByName,
        Table::TokenById,
        Table::Nodes,
        Table::Relationships,
        Table::Outgoing,
        Table::Incoming,
        Table::LabelIndex,
        Table::Indexes,
        Table::KeywordPostings,
        Table::KeywordDocuments,
        Table::KeywordTotals,
        Table::Vectors,
        Table::PropertyEntries,
    ];

    pub(crate) const fn name(self) -> &'static str {
        match self {
            Table::Metadata => "metadata",
            Table::TokenByName => "token_by_name",
            Table::TokenById => "token_by_id",
            Table::Nodes => "nodes",
            Table::Relationships => "relationships",
            Table::Outgoing => "outgoing",
            Table::Incoming => "incoming",
            Table::LabelIndex => "label_index",
            Table::Indexes => "indexes",
            Table::KeywordPostings => "keyword_postings",
            Table::KeywordDocuments => "keyword_documents",
            Table::KeywordTotals => "keyword_totals",
            Table::Vectors => "vectors",
            Table::PropertyEntries => "property_entries",
        }
    }
}

pub(crate) type Entry = (Vec<u8>, Vec<u8>);

/// A failure reported by the storage engine, kept opaque so no engine type leaks upward.
#[derive(Debug)]
pub(crate) struct StorageError {
    source: Box<dyn Error + Send + Sync>,
}

impl StorageError {
    pub(crate) fn new(source: impl Error + Send + Sync + 'static) -> Self {
        Self {
            source: Box::new(source),
        }
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
}

pub(crate) trait ReadTransaction {
    type Cursor<'a>: Cursor
    where
        Self: 'a;

    /// A cursor that reads entries of `table` by key, cheapest when the keys come in ascending
    /// order.
    fn cursor(&self, table: Table) -> Result<Self::Cursor<'_>, StorageError>;

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
}

/// How a cursor reaches the key it reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Reach {
    /// Steps forward from the cursor's place, for a key just after the one it reached last.
    Step,
    /// Moves the cursor's place to the key with a search from the root.
    Seek,
    /// Looks the key up and leaves the cursor's place where it is.
    Lookup,
}

/// Reads the entries of one table by key. A cursor may keep the values it read last at hand, so
/// reading one of their keys again costs nothing.
pub(crate) trait Cursor {
    /// Calls `read` with the value stored under `key`, without copying it.
    fn get_with<R, E: From<StorageError>>(
        &mut self,
        key: &[u8],
        reach: Reach,
        read: impl FnOnce(&[u8]) -> Result<R, E>,
    ) -> Result<Option<R>, E>;
}

/// A write transaction sees its own writes. Dropping it without commit discards them.
pub(crate) trait WriteTransaction: ReadTransaction {
    fn put(&mut self, table: Table, key: &[u8], value: &[u8]) -> Result<(), StorageError>;

    fn delete(&mut self, table: Table, key: &[u8]) -> Result<(), StorageError>;

    fn commit(self) -> Result<(), StorageError>;

    fn rollback(self) -> Result<(), StorageError>;
}
