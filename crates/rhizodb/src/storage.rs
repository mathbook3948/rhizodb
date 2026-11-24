use std::error::Error;
use std::fmt;

#[cfg(test)]
pub(crate) mod conformance;
#[cfg(test)]
pub(crate) mod model;
pub(crate) mod redb_backend;
#[cfg(test)]
pub(crate) mod sim_backend;

/// The fixed set of ordered key-value tables the graph layer is built on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum Table {
    Metadata,
    TokenByName,
    TokenById,
    /// The node records of format versions before property columns, which only their upgrade
    /// reads.
    Nodes,
    Relationships,
    Outgoing,
    Incoming,
    /// The label index of format versions before node meta, which only their upgrade reads.
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
}

impl Table {
    pub(crate) const ALL: [Table; 18] = [
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
        Table::NodeChunks,
        Table::LabelSets,
        Table::LabelSetByTokens,
        Table::NodeColumns,
    ];

    /// Whether only the upgrade of an older format version reads the table, so a new database
    /// does not create it.
    pub(crate) const fn is_legacy(self) -> bool {
        matches!(self, Table::LabelIndex | Table::Nodes)
    }

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
            Table::NodeChunks => "node_chunks",
            Table::LabelSets => "label_sets",
            Table::LabelSetByTokens => "label_set_by_tokens",
            Table::NodeColumns => "node_columns",
        }
    }
}

pub(crate) type Entry = (Vec<u8>, Vec<u8>);

/// A failure reported by the storage engine, kept opaque so no engine type leaks upward.
#[derive(Debug)]
pub(crate) struct StorageError {
    source: Box<dyn Error + Send + Sync>,
    corrupt: bool,
    unsupported: bool,
}

impl StorageError {
    pub(crate) fn new(source: impl Error + Send + Sync + 'static) -> Self {
        Self {
            source: Box::new(source),
            corrupt: false,
            unsupported: false,
        }
    }

    /// A failure the engine raised on damaged stored data, which is reported as corrupt data.
    pub(crate) fn corrupt(source: impl Error + Send + Sync + 'static) -> Self {
        Self {
            corrupt: true,
            ..Self::new(source)
        }
    }

    pub(crate) fn is_corrupt(&self) -> bool {
        self.corrupt
    }

    /// An operation this backend does not offer.
    pub(crate) fn unsupported(source: impl Error + Send + Sync + 'static) -> Self {
        Self {
            unsupported: true,
            ..Self::new(source)
        }
    }

    pub(crate) fn is_unsupported(&self) -> bool {
        self.unsupported
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
