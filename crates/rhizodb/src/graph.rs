use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::btree_map;
use std::error::Error;
use std::fmt;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::PoisonError;
use std::sync::RwLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::thread;
use std::thread::ThreadId;

use crate::storage::Peek;
use crate::storage::ReadTransaction;
use crate::storage::Storage;
use crate::storage::StorageError;
use crate::storage::Table;
use crate::storage::WriteTransaction;
use crate::value::DecodeError;
use crate::value::PropertyValue;
use crate::value::Reader;

mod adjacency;
mod budget;
mod columns;
mod dump;
mod ids;
mod index;
#[cfg(test)]
pub(crate) mod integrity;
mod keys;
mod meta;
mod postings;
mod properties;
mod records;
mod relationships;
mod tokenize;
mod value_key;

pub(crate) use adjacency::AdjacencyReader;
pub(crate) use columns::Cell;
pub(crate) use columns::ColumnReader;
pub(crate) use columns::Quick;
pub(crate) use columns::Text;
pub(crate) use ids::Label;
pub use ids::NodeId;
pub(crate) use ids::PropertyKey;
pub use ids::RelationshipId;
pub(crate) use ids::RelationshipType;
pub(crate) use ids::Token;
pub(crate) use ids::TokenKind;
pub(crate) use index::ElementKind;
pub(crate) use index::IndexDefinition;
pub(crate) use index::IndexKind;
pub use index::IndexTarget;
pub(crate) use index::Target;
pub use index::VectorMetric;
pub(crate) use meta::LabelReader;
#[cfg(test)]
pub(crate) use meta::SLOTS;
pub(crate) use meta::row_group;
pub(crate) use properties::ScanGroup;
pub(crate) use records::NodeRecord;
pub(crate) use records::Properties;
pub(crate) use records::RelationshipRecord;
pub(crate) use value_key::Bound;
pub(crate) use value_key::Compared;
pub(crate) use value_key::KeyRange;
pub(crate) use value_key::compared;

use adjacency::Edge;
use adjacency::Entries;
use adjacency::Entry;
use adjacency::Lazy;
use adjacency::SegmentEdit;
use adjacency::SegmentId;
use adjacency::Side;
use adjacency::SlotRead;
use columns::ColumnsEdit;
use index::Element;
use meta::Delta;
use meta::Flush;
use meta::GroupEdit;
use meta::LabelSetCache;
use meta::LabelSetId;
use meta::Meta;
use meta::MetaBytes;
use meta::MetaHead;
use meta::Part;
use meta::Slot;
use relationships::ChunkEdit;
use relationships::ChunkRef;

const FORMAT_VERSION: u32 = 14;

/// The graph format a write of `value` under `key` of the metadata table records, when it is
/// newer than this build reads.
pub(crate) fn newer_format_put(key: &[u8], value: &[u8]) -> Option<u32> {
    if key != keys::FORMAT_VERSION_KEY {
        return None;
    }
    let version = u32::from_be_bytes(value.try_into().ok()?);
    (version > FORMAT_VERSION).then_some(version)
}

/// The graph format a write of `value` under `key` of the metadata table leaves for its writer
/// to upgrade: an older format version, or this build's for another tokenizer.
pub(crate) fn older_format_put(key: &[u8], value: &[u8]) -> Option<u32> {
    if key == keys::FORMAT_VERSION_KEY {
        let version = u32::from_be_bytes(value.try_into().ok()?);
        return (version < FORMAT_VERSION).then_some(version);
    }
    (key == keys::TOKENIZER_KEY && value != tokenize::fingerprint().as_slice())
        .then_some(FORMAT_VERSION)
}
const OLDEST_FORMAT_VERSION: u32 = 11;
const RELATIONSHIP_CHUNKS_FORMAT_VERSION: u32 = 12;
const POSTING_BLOCKS_FORMAT_VERSION: u32 = 14;
const HELD_RELATIONSHIP_BYTES: usize = 4 << 20;
const ENTRIES_OUT_OF_ORDER: GraphError =
    GraphError::Corrupt("the adjacency entries of a segment are out of order");

#[derive(Debug)]
pub(crate) enum GraphError {
    Storage(StorageError),
    Decode(DecodeError),
    Corrupt(&'static str),
    UnsupportedFormatVersion(u32),
    UpgradePending(u32),
    IdSpaceExhausted,
    NodeNotFound(NodeId),
    RelationshipNotFound(RelationshipId),
    NodeHasRelationships(NodeId),
    WriterOnThisThread,
    IndexNotFound(String, &'static str),
    IndexExists(String),
    InvalidIndex(&'static str),
    InvalidQueryVector(String),
    NotWriter(&'static str),
    ReadOnly(&'static str),
}

/// What a graph may write as it opens its storage and afterwards.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Access {
    /// Records the format version and upgrades an older format, and takes local writes.
    Writer,
    /// Refuses every local write, and so fails instead of writing an upgrade.
    Replica,
    /// Over storage that takes no write at all, which fails instead of writing an upgrade.
    ReadOnly,
}

impl fmt::Display for GraphError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Storage(error) => write!(formatter, "{error}"),
            Self::Decode(error) => write!(formatter, "stored data is unreadable: {error}"),
            Self::Corrupt(what) => write!(formatter, "stored data is inconsistent: {what}"),
            Self::UnsupportedFormatVersion(found) => {
                write!(formatter, "unsupported database format version {found}")
            }
            Self::UpgradePending(found) if *found == FORMAT_VERSION => write!(
                formatter,
                "the database's keyword indexes follow another tokenizer, which this build reads \
                 only once the database's writer rebuilds them"
            ),
            Self::UpgradePending(found) => write!(
                formatter,
                "the database holds graph format {found}, which this build reads only once the \
                 database's writer upgrades it to format {FORMAT_VERSION}"
            ),
            Self::IdSpaceExhausted => write!(formatter, "no identifiers left to allocate"),
            Self::NodeNotFound(node) => write!(formatter, "node {} does not exist", node.0),
            Self::RelationshipNotFound(relationship) => {
                write!(formatter, "relationship {} does not exist", relationship.0)
            }
            Self::NodeHasRelationships(node) => {
                write!(formatter, "node {} still has relationships", node.0)
            }
            Self::WriterOnThisThread => write!(
                formatter,
                "this thread already holds the open write transaction, so another would wait forever"
            ),
            Self::IndexNotFound(name, kind) => {
                write!(formatter, "there is no {kind} named `{name}`")
            }
            Self::IndexExists(name) => write!(
                formatter,
                "an index named `{name}` already exists with another definition"
            ),
            Self::InvalidIndex(reason) => write!(formatter, "invalid index: {reason}"),
            Self::InvalidQueryVector(reason) => write!(formatter, "invalid query vector: {reason}"),
            Self::NotWriter(what) => {
                write!(formatter, "a database that is not the writer cannot {what}")
            }
            Self::ReadOnly(what) => write!(
                formatter,
                "a database opened read-only cannot {what}; open it once for writing to do so"
            ),
        }
    }
}

impl Error for GraphError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Storage(error) => Some(error),
            Self::Decode(error) => Some(error),
            _ => None,
        }
    }
}

impl From<StorageError> for GraphError {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}

impl From<DecodeError> for GraphError {
    fn from(error: DecodeError) -> Self {
        Self::Decode(error)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Direction {
    Outgoing,
    Incoming,
    Both,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Adjacent {
    pub(crate) relationship: RelationshipId,
    pub(crate) rel_type: Token<RelationshipType>,
    pub(crate) other: NodeId,
}

#[cfg(test)]
pub(crate) struct ReadHook(Option<Arc<dyn Fn(u8) + Send + Sync>>);

#[cfg(test)]
impl fmt::Debug for ReadHook {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ReadHook")
    }
}

#[derive(Debug)]
pub(crate) struct Graph<S> {
    storage: S,
    writers: Arc<Writers>,
    replica: AtomicBool,
    pending: AtomicU32,
    upgrades: AtomicU64,
    #[cfg(test)]
    read_hook: Mutex<ReadHook>,
    committed_index_definitions: Arc<CommittedCatalog>,
    committed_tokens: Arc<CommittedTokens>,
    committed_label_sets: Arc<LabelSetCache>,
    committed_column_deltas: Arc<ColumnDeltas>,
}

type ColumnDeltas = Mutex<BTreeMap<u64, usize>>;

/// The tokens of every name space by name and by ID, as committed. A committed token never
/// changes, and no element of a snapshot taken before a token was committed refers to it.
#[derive(Debug, Default)]
pub(crate) struct CommittedTokens {
    spaces: RwLock<[TokenSpace; 3]>,
}

#[derive(Debug, Default)]
struct TokenSpace {
    by_name: HashMap<Arc<str>, u32>,
    by_id: HashMap<u32, Arc<str>>,
}

impl CommittedTokens {
    fn get<K: TokenKind>(&self, name: &str) -> Option<Token<K>> {
        let spaces = self.spaces.read().unwrap_or_else(PoisonError::into_inner);
        spaces
            .get(usize::from(K::TAG))?
            .by_name
            .get(name)
            .map(|id| Token::new(*id))
    }

    fn name<K: TokenKind>(&self, token: Token<K>) -> Option<Arc<str>> {
        let spaces = self.spaces.read().unwrap_or_else(PoisonError::into_inner);
        spaces
            .get(usize::from(K::TAG))?
            .by_id
            .get(&token.id())
            .cloned()
    }

    fn insert<K: TokenKind>(&self, name: Arc<str>, token: Token<K>) {
        let mut spaces = self.spaces.write().unwrap_or_else(PoisonError::into_inner);
        if let Some(space) = spaces.get_mut(usize::from(K::TAG)) {
            space.by_id.insert(token.id(), Arc::clone(&name));
            space.by_name.insert(name, token.id());
        }
    }
}

impl<S: Storage> Graph<S> {
    /// Records the format version on first open, upgrades a database written in an older
    /// version to the current one, and rejects a database written in a newer version or one
    /// older than the oldest it upgrades. Keyword indexes built by another tokenizer are rebuilt
    /// in the same write transaction.
    #[cfg(test)]
    pub(crate) fn open(storage: S) -> Result<Self, GraphError> {
        Self::open_as(storage, Access::Writer)
    }

    /// Opens the graph as [`Graph::open`] does, or, for any `access` but a writer's, without
    /// writing the format version or an upgrade: a replica of an older format or tokenizer opens
    /// waiting for its writer's upgrade, and a graph opened read-only reads the current format
    /// and tokenizer only, since the graph reads no older layout as it is.
    pub(crate) fn open_as(storage: S, access: Access) -> Result<Self, GraphError> {
        if access == Access::ReadOnly {
            let tx = GraphTx::bare(storage.begin_read()?);
            let found = Self::stored_format(&tx)?;
            if found.is_some_and(|found| found != FORMAT_VERSION) {
                return Err(GraphError::ReadOnly(
                    "upgrade a database written in an older format",
                ));
            }
            if found.is_some() && !tx.tokenizer_is_current()? {
                return Err(GraphError::ReadOnly(
                    "rebuild keyword indexes for this build's tokenizer",
                ));
            }
        }
        let replica = access == Access::Replica;
        let pending = match access {
            Access::Replica => Self::pending_format(&storage)?,
            Access::Writer => {
                Self::upgrade(&storage)?;
                None
            }
            Access::ReadOnly => None,
        };
        let definitions = match pending {
            Some(_) => Arc::from(Vec::new()),
            None => GraphTx::bare(storage.begin_read()?).index_definitions()?,
        };
        Ok(Self {
            storage,
            writers: Arc::default(),
            replica: AtomicBool::new(replica),
            pending: AtomicU32::new(pending.unwrap_or(0)),
            upgrades: AtomicU64::new(0),
            #[cfg(test)]
            read_hook: Mutex::new(ReadHook(None)),
            committed_index_definitions: Arc::new(CommittedCatalog {
                definitions: Mutex::new(definitions),
                changes: AtomicU64::new(0),
                publishing: Mutex::new(()),
            }),
            committed_tokens: Arc::default(),
            committed_label_sets: Arc::default(),
            committed_column_deltas: Arc::default(),
        })
    }

    fn stored_format(tx: &GraphTx<impl ReadTransaction>) -> Result<Option<u32>, GraphError> {
        let found = match tx
            .transaction
            .get(Table::Metadata, keys::FORMAT_VERSION_KEY)?
        {
            None => None,
            Some(bytes) => Some(decode_u32(&bytes)?),
        };
        match found {
            Some(found)
                if found != FORMAT_VERSION
                    && !(OLDEST_FORMAT_VERSION..FORMAT_VERSION).contains(&found) =>
            {
                Err(GraphError::UnsupportedFormatVersion(found))
            }
            found => Ok(found),
        }
    }

    fn pending_format(storage: &S) -> Result<Option<u32>, GraphError> {
        Self::pending_in(&GraphTx::bare(storage.begin_read()?))
    }

    fn pending_in(tx: &GraphTx<impl ReadTransaction>) -> Result<Option<u32>, GraphError> {
        let Some(found) = Self::stored_format(tx)? else {
            return Ok(None);
        };
        Ok((found != FORMAT_VERSION || !tx.tokenizer_is_current()?).then_some(found))
    }

    fn upgrade(storage: &S) -> Result<(), GraphError> {
        let rewrote = {
            let mut tx = GraphTx::bare(storage.begin_write()?);
            let found = Self::stored_format(&tx)?;
            let upgrade = found != Some(FORMAT_VERSION);
            if let Some(found) = found
                && found < RELATIONSHIP_CHUNKS_FORMAT_VERSION
            {
                tx.move_relationships_to_chunks()?;
            }
            if found.is_some_and(|found| found < POSTING_BLOCKS_FORMAT_VERSION) {
                tx.move_postings_to_blocks()?;
            }
            let changed = tx.adopt_current_tokenizer()?;
            if upgrade {
                tx.transaction.put(
                    Table::Metadata,
                    keys::FORMAT_VERSION_KEY,
                    &FORMAT_VERSION.to_be_bytes(),
                )?;
            }
            if changed || upgrade {
                tx.commit()?;
            }
            found.is_some() && (changed || upgrade)
        };
        if rewrote {
            storage.checkpoint()?;
        }
        Ok(())
    }

    fn ready(&self) -> Result<(), GraphError> {
        match self.pending.load(Ordering::Acquire) {
            0 => Ok(()),
            found => Err(GraphError::UpgradePending(found)),
        }
    }

    /// Makes the graph wait for its writer's upgrade of the graph format `found` before it
    /// serves reads, ahead of changes that leave it in that format.
    ///
    /// It marks the graph pending before it counts the wait, both before the apply publishes the
    /// format, so a read that loads the count after it changed finds the graph pending at its
    /// first check, and a read that loaded it before finds it changed once its snapshot holds the
    /// format and checks that snapshot.
    pub(crate) fn wait_for_upgrade(&self, found: u32) {
        self.pending.store(found, Ordering::SeqCst);
        #[cfg(test)]
        self.reached(2);
        self.upgrades.fetch_add(1, Ordering::SeqCst);
    }

    #[cfg(test)]
    /// Calls `hook` with 0 after a read checks that the graph is ready, with 1 after it begins
    /// its snapshot, and with 2 between the two steps of `wait_for_upgrade`, so a test can run
    /// an apply or a read between them.
    pub(crate) fn hook_reads(&self, hook: Arc<dyn Fn(u8) + Send + Sync>) {
        lock(&self.read_hook).0 = Some(hook);
    }

    #[cfg(test)]
    fn reached(&self, point: u8) {
        let hook = lock(&self.read_hook).0.clone();
        if let Some(hook) = hook {
            hook(point);
        }
    }

    /// Whether the graph waits for its writer's upgrade before it serves reads.
    pub(crate) fn upgrade_pending(&self) -> bool {
        self.pending.load(Ordering::Acquire) != 0
    }

    /// Ends the wait for an upgrade once the file holds this build's format and tokenizer: as
    /// the writer it upgrades the graph itself, and otherwise it reads whether the writer's
    /// upgrade arrived. The index definitions are reloaded before any reader begins.
    pub(crate) fn finish_pending_upgrade(&self, as_writer: bool) -> Result<(), GraphError> {
        if !self.upgrade_pending() {
            return Ok(());
        }
        if as_writer {
            Self::upgrade(&self.storage)?;
        } else if let Some(found) = Self::pending_format(&self.storage)? {
            self.pending.store(found, Ordering::Release);
            return Ok(());
        }
        let committed = &self.committed_index_definitions;
        let publishing = lock(&committed.publishing);
        committed.changes.fetch_add(1, Ordering::SeqCst);
        let reloaded = self
            .storage
            .begin_read()
            .map_err(GraphError::from)
            .and_then(|read| GraphTx::bare(read).index_definitions());
        if let Ok(definitions) = &reloaded {
            *lock(&committed.definitions) = Arc::clone(definitions);
        }
        committed.changes.fetch_add(1, Ordering::SeqCst);
        drop(publishing);
        reloaded?;
        lock(&self.committed_column_deltas).clear();
        self.pending.store(0, Ordering::Release);
        Ok(())
    }

    pub(crate) fn storage(&self) -> &S {
        &self.storage
    }

    /// Copies the committed graph, as one read snapshot sees it, into a new database file at
    /// `path`, which opens as this graph.
    pub(crate) fn backup_to(&self, path: &std::path::Path) -> Result<(), GraphError> {
        Ok(crate::storage::backup::write(&self.storage, path)?)
    }

    pub(crate) fn read(&self) -> Result<GraphTx<S::Read<'_>>, GraphError> {
        let upgrades = self.upgrades.load(Ordering::SeqCst);
        self.ready()?;
        #[cfg(test)]
        self.reached(0);
        let committed = &self.committed_index_definitions;
        let before = committed.changes.load(Ordering::SeqCst);
        let transaction = self.storage.begin_read()?;
        #[cfg(test)]
        self.reached(1);
        self.ready()?;
        let waited = self.upgrades.load(Ordering::SeqCst) != upgrades;
        let definitions = Arc::clone(&lock(&committed.definitions));
        let index_definitions =
            if before.is_multiple_of(2) && committed.changes.load(Ordering::SeqCst) == before {
                OnceLock::from(definitions)
            } else {
                OnceLock::new()
            };
        let tx = GraphTx {
            transaction,
            _writer: None,
            index_definitions,
            catalog: None,
            tokens: Arc::clone(&self.committed_tokens),
            reads_committed_tokens: true,
            own_tokens: CommittedTokens::default(),
            label_sets: Arc::clone(&self.committed_label_sets),
            own_label_sets: LabelSetCache::default(),
            new_label_sets: BTreeMap::new(),
            meta: BTreeMap::new(),
            columns: BTreeMap::new(),
            reader: ColumnReader::default(),
            labels: LabelReader::default(),
            column_deltas: None,
            own_column_deltas: BTreeMap::new(),
            adjacency: BTreeMap::new(),
            relationship_chunks: BTreeMap::new(),
            relationship_bytes: 0,
            record_buffer: Vec::new(),
            postings: postings::Held::default(),
            counters: Vec::new(),
        };
        if waited && let Some(found) = Self::pending_in(&tx)? {
            return Err(GraphError::UpgradePending(found));
        }
        Ok(tx)
    }

    /// Records the format version and the tokenizer in a graph that holds neither, as a
    /// replica that never applied a change leaves it, so a replica promoted before its first
    /// change writes what opening a new database as a writer writes.
    pub(crate) fn record_format_if_missing(&self) -> Result<(), GraphError> {
        let mut tx = self.write()?;
        if tx
            .transaction
            .get(Table::Metadata, keys::FORMAT_VERSION_KEY)?
            .is_some()
        {
            return tx.rollback();
        }
        tx.transaction.put(
            Table::Metadata,
            keys::FORMAT_VERSION_KEY,
            &FORMAT_VERSION.to_be_bytes(),
        )?;
        tx.adopt_current_tokenizer()?;
        tx.commit()
    }

    /// Makes the graph a replica that refuses local writes, or a writer that takes them. A
    /// write transaction that began before stays open, so a caller that needs every write to
    /// have ended waits for the storage's writer after this.
    pub(crate) fn set_replica(&self, replica: bool) {
        if !replica {
            lock(&self.committed_column_deltas).clear();
        }
        self.replica.store(replica, Ordering::Release);
    }

    /// Runs `work`, which changes the storage outside a graph transaction, and then reloads the
    /// index definitions when `indexes` says the work may have changed them. Readers that begin
    /// meanwhile read the definitions from their own snapshot, so none sees data and
    /// definitions of different commits.
    pub(crate) fn apply_outside<R>(
        &self,
        indexes: bool,
        work: impl FnOnce(&S) -> R,
    ) -> Result<R, GraphError> {
        if lock(&self.writers.slot).writer == Some(thread::current().id()) {
            return Err(GraphError::WriterOnThisThread);
        }
        lock(&self.committed_column_deltas).clear();
        if self.upgrade_pending() {
            let result = work(&self.storage);
            self.finish_pending_upgrade(false)?;
            return Ok(result);
        }
        if !indexes {
            return Ok(work(&self.storage));
        }
        let committed = &self.committed_index_definitions;
        let publishing = lock(&committed.publishing);
        committed.changes.fetch_add(1, Ordering::SeqCst);
        let result = work(&self.storage);
        let reloaded = self
            .storage
            .begin_read()
            .map_err(GraphError::from)
            .and_then(|read| GraphTx::bare(read).index_definitions());
        if let Ok(definitions) = &reloaded {
            *lock(&committed.definitions) = Arc::clone(definitions);
        }
        committed.changes.fetch_add(1, Ordering::SeqCst);
        drop(publishing);
        reloaded?;
        Ok(result)
    }

    /// Runs `work` on the storage, which may take the storage's writer itself, and fails when
    /// this thread holds the write transaction that work would wait for.
    pub(crate) fn maintain<R>(&self, work: impl FnOnce(&S) -> R) -> Result<R, GraphError> {
        if lock(&self.writers.slot).writer == Some(thread::current().id()) {
            return Err(GraphError::WriterOnThisThread);
        }
        Ok(work(&self.storage))
    }

    /// Runs `work` on the storage, which must take no writer, from any thread.
    pub(crate) fn inspect<R>(&self, work: impl FnOnce(&S) -> R) -> R {
        work(&self.storage)
    }

    /// Keeps write transactions of other threads from beginning until the claim is dropped,
    /// waiting for another thread's claim to end. The calling thread goes on beginning write
    /// transactions one after another. Fails when the calling thread holds a write transaction.
    pub(crate) fn exclusive_writes(&self) -> Result<ExclusiveWrites, GraphError> {
        let current = thread::current().id();
        let mut slot = lock(&self.writers.slot);
        if slot.writer == Some(current) {
            return Err(GraphError::WriterOnThisThread);
        }
        while slot.exclusive.is_some() {
            slot = wait(&self.writers.released, slot);
        }
        slot.exclusive = Some(current);
        Ok(ExclusiveWrites {
            writers: Arc::clone(&self.writers),
        })
    }

    /// Opens the one write transaction, waiting for another thread's to end and for another
    /// thread's exclusive writes. Fails instead of waiting forever when the calling thread
    /// already holds it.
    pub(crate) fn write(&self) -> Result<GraphTx<S::Write<'_>>, GraphError> {
        if self.replica.load(Ordering::Acquire) {
            return Err(GraphError::NotWriter("write to the graph"));
        }
        self.ready()?;
        let current = thread::current().id();
        let transaction = loop {
            {
                let mut slot = lock(&self.writers.slot);
                if slot.writer == Some(current) {
                    return Err(GraphError::WriterOnThisThread);
                }
                while slot.excludes(current) {
                    slot = wait(&self.writers.released, slot);
                }
            }
            let transaction = self.storage.begin_write()?;
            if self.replica.load(Ordering::Acquire) {
                transaction.rollback()?;
                return Err(GraphError::NotWriter("write to the graph"));
            }
            let mut slot = lock(&self.writers.slot);
            if !slot.excludes(current) {
                slot.writer = Some(current);
                break transaction;
            }
            drop(slot);
            transaction.rollback()?;
        };
        let committed = &self.committed_index_definitions;
        let index_definitions = if committed.changes.load(Ordering::SeqCst).is_multiple_of(2) {
            OnceLock::from(Arc::clone(&lock(&committed.definitions)))
        } else {
            OnceLock::new()
        };
        Ok(GraphTx {
            transaction,
            _writer: Some(WriterClaim {
                writers: Arc::clone(&self.writers),
                owner: current,
            }),
            index_definitions,
            catalog: Some(Catalog {
                shared: Arc::clone(&self.committed_index_definitions),
                changed: false,
            }),
            tokens: Arc::clone(&self.committed_tokens),
            reads_committed_tokens: false,
            own_tokens: CommittedTokens::default(),
            label_sets: Arc::clone(&self.committed_label_sets),
            own_label_sets: LabelSetCache::default(),
            new_label_sets: BTreeMap::new(),
            meta: BTreeMap::new(),
            columns: BTreeMap::new(),
            reader: ColumnReader::default(),
            labels: LabelReader::default(),
            column_deltas: Some(Arc::clone(&self.committed_column_deltas)),
            own_column_deltas: BTreeMap::new(),
            adjacency: BTreeMap::new(),
            relationship_chunks: BTreeMap::new(),
            relationship_bytes: 0,
            record_buffer: Vec::new(),
            postings: postings::Held::default(),
            counters: Vec::new(),
        })
    }
}

/// The graph operations over one storage transaction. Reads need a read transaction, writes a write transaction.
#[derive(Debug)]
pub(crate) struct GraphTx<T> {
    transaction: T,
    _writer: Option<WriterClaim>,
    index_definitions: OnceLock<Arc<[Vec<u8>]>>,
    catalog: Option<Catalog>,
    tokens: Arc<CommittedTokens>,
    reads_committed_tokens: bool,
    own_tokens: CommittedTokens,
    label_sets: Arc<LabelSetCache>,
    own_label_sets: LabelSetCache,
    new_label_sets: BTreeMap<LabelSetId, (Vec<u8>, usize)>,
    meta: BTreeMap<u64, GroupEdit>,
    columns: BTreeMap<u64, ColumnsEdit>,
    reader: ColumnReader,
    labels: LabelReader,
    column_deltas: Option<Arc<ColumnDeltas>>,
    own_column_deltas: BTreeMap<u64, usize>,
    adjacency: BTreeMap<SegmentId, SegmentEdit>,
    relationship_chunks: BTreeMap<u64, ChunkEdit>,
    relationship_bytes: usize,
    record_buffer: Vec<u8>,
    postings: postings::Held,
    counters: Vec<(Vec<u8>, u64)>,
}

impl<T> GraphTx<T> {
    fn bare(transaction: T) -> Self {
        Self {
            transaction,
            _writer: None,
            index_definitions: OnceLock::new(),
            catalog: None,
            tokens: Arc::default(),
            reads_committed_tokens: false,
            own_tokens: CommittedTokens::default(),
            label_sets: Arc::default(),
            own_label_sets: LabelSetCache::default(),
            new_label_sets: BTreeMap::new(),
            meta: BTreeMap::new(),
            columns: BTreeMap::new(),
            reader: ColumnReader::default(),
            labels: LabelReader::default(),
            column_deltas: None,
            own_column_deltas: BTreeMap::new(),
            adjacency: BTreeMap::new(),
            relationship_chunks: BTreeMap::new(),
            relationship_bytes: 0,
            record_buffer: Vec::new(),
            postings: postings::Held::default(),
            counters: Vec::new(),
        }
    }
}

#[derive(Debug)]
struct Catalog {
    shared: Arc<CommittedCatalog>,
    changed: bool,
}

#[derive(Debug)]
struct CommittedCatalog {
    definitions: Mutex<Arc<[Vec<u8>]>>,
    changes: AtomicU64,
    publishing: Mutex<()>,
}

#[derive(Debug, Default)]
struct Writers {
    slot: Mutex<WriterSlot>,
    released: Condvar,
}

#[derive(Debug, Default)]
struct WriterSlot {
    writer: Option<ThreadId>,
    exclusive: Option<ThreadId>,
}

impl WriterSlot {
    fn excludes(&self, thread: ThreadId) -> bool {
        self.exclusive.is_some_and(|owner| owner != thread)
    }
}

#[derive(Debug)]
struct WriterClaim {
    writers: Arc<Writers>,
    owner: ThreadId,
}

impl Drop for WriterClaim {
    fn drop(&mut self) {
        let mut slot = lock(&self.writers.slot);
        if slot.writer == Some(self.owner) {
            slot.writer = None;
        }
    }
}

/// Write transactions of other threads wait while it lives.
#[derive(Debug)]
pub(crate) struct ExclusiveWrites {
    writers: Arc<Writers>,
}

impl Drop for ExclusiveWrites {
    fn drop(&mut self) {
        lock(&self.writers.slot).exclusive = None;
        self.writers.released.notify_all();
    }
}

fn wait<'a, V>(
    condition: &Condvar,
    guard: std::sync::MutexGuard<'a, V>,
) -> std::sync::MutexGuard<'a, V> {
    condition
        .wait(guard)
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn lock<V>(value: &Mutex<V>) -> std::sync::MutexGuard<'_, V> {
    value
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn decode_u32(bytes: &[u8]) -> Result<u32, DecodeError> {
    let mut reader = Reader::new(bytes);
    let value = reader.u32()?;
    reader.finish()?;
    Ok(value)
}

fn decode_u64(bytes: &[u8]) -> Result<u64, DecodeError> {
    let mut reader = Reader::new(bytes);
    let value = reader.u64()?;
    reader.finish()?;
    Ok(value)
}

impl<T: ReadTransaction> GraphTx<T> {
    #[cfg(test)]
    /// The stored graph format version this transaction sees.
    pub(crate) fn stored_format_version(&self) -> Option<Vec<u8>> {
        self.transaction
            .get(Table::Metadata, keys::FORMAT_VERSION_KEY)
            .ok()
            .flatten()
    }

    #[cfg(test)]
    pub(crate) fn node(&self, id: NodeId) -> Result<Option<NodeRecord>, GraphError> {
        let Some(labels) = self.node_labels(id)? else {
            return Ok(None);
        };
        let properties = self.live_node_properties(&mut ColumnReader::default(), id)?;
        Ok(Some(NodeRecord {
            labels: labels.iter().copied().collect(),
            properties,
        }))
    }

    /// One property of a relationship, decoding none of the others. The outer `None` means the
    /// relationship does not exist.
    pub(crate) fn relationship_property(
        &self,
        id: RelationshipId,
        key: Token<PropertyKey>,
    ) -> Result<Option<Option<PropertyValue>>, GraphError> {
        self.with_relationship(id, |bytes| Ok(RelationshipRecord::property(bytes, key)?))
    }

    /// The type of a relationship, decoding none of its properties. `None` means the
    /// relationship does not exist.
    pub(crate) fn relationship_type(
        &self,
        id: RelationshipId,
    ) -> Result<Option<Token<RelationshipType>>, GraphError> {
        self.with_relationship(id, |bytes| Ok(RelationshipRecord::rel_type(bytes)?))
    }

    /// The property keys of a relationship, ascending, decoding none of its values. `None`
    /// means the relationship does not exist.
    pub(crate) fn relationship_keys(
        &self,
        id: RelationshipId,
    ) -> Result<Option<Vec<Token<PropertyKey>>>, GraphError> {
        self.with_relationship(id, |bytes| Ok(RelationshipRecord::keys(bytes)?))
    }

    pub(crate) fn relationship(
        &self,
        id: RelationshipId,
    ) -> Result<Option<RelationshipRecord>, GraphError> {
        self.with_relationship(id, |bytes| Ok(RelationshipRecord::decode(bytes)?))
    }

    fn with_relationship<R>(
        &self,
        id: RelationshipId,
        read: impl FnOnce(&[u8]) -> Result<R, GraphError>,
    ) -> Result<Option<R>, GraphError> {
        let (chunk, slot) = relationships::locate(id);
        if let Some(edit) = self.relationship_chunks.get(&chunk) {
            return edit.get(slot).map(read).transpose();
        }
        Ok(self
            .transaction
            .get_with(
                Table::RelationshipChunks,
                &relationships::chunk_key(chunk),
                |bytes| ChunkRef::parse(bytes)?.get(slot)?.map(read).transpose(),
            )?
            .flatten())
    }

    /// Calls `visit` with every relationship's ID and stored record, in ascending ID order, as
    /// this transaction has left them.
    pub(crate) fn visit_relationship_records(
        &self,
        mut visit: impl FnMut(RelationshipId, &[u8]) -> Result<(), GraphError>,
    ) -> Result<(), GraphError> {
        let mut held = self.relationship_chunks.iter().peekable();
        self.transaction
            .visit_prefix(Table::RelationshipChunks, &[], |key, value| {
                let chunk = relationships::parse_key(key)?;
                while let Some((&at, edit)) = held.next_if(|(at, _)| **at <= chunk) {
                    visit_held(at, edit, &mut visit)?;
                    if at == chunk {
                        return Ok(());
                    }
                }
                for entry in ChunkRef::parse(value)?.entries() {
                    let (slot, record) = entry?;
                    visit(relationships::relationship_at(chunk, slot), record)?;
                }
                Ok::<(), GraphError>(())
            })?;
        held.try_for_each(|(&chunk, edit)| visit_held(chunk, edit, &mut visit))
    }

    /// The labels of the label set `set`.
    pub(crate) fn label_set(&self, set: LabelSetId) -> Result<Arc<[Token<Label>]>, GraphError> {
        if set == LabelSetId::EMPTY {
            return Ok(Arc::from([]));
        }
        if let Some(labels) = self
            .label_sets
            .labels(set)
            .or_else(|| self.own_label_sets.labels(set))
        {
            return Ok(labels);
        }
        let labels = self
            .transaction
            .get_with(
                Table::LabelSets,
                &set.0.to_be_bytes(),
                meta::parse_label_set,
            )?
            .ok_or(GraphError::Corrupt(
                "a node has a label set that does not exist",
            ))?;
        if self.reads_committed_tokens {
            self.label_sets.insert(set, Arc::clone(&labels));
        } else {
            self.own_label_sets.insert(set, Arc::clone(&labels));
        }
        Ok(labels)
    }

    fn visit_meta(
        &self,
        check_deltas: bool,
        visit: impl FnMut(Meta<'_>) -> Result<(), GraphError>,
    ) -> Result<(), GraphError> {
        self.visit_meta_from(check_deltas, 0, visit)
    }

    /// [`visit_meta`](Self::visit_meta) over the row groups from `from` on.
    pub(crate) fn visit_meta_from<E: From<GraphError> + From<StorageError>>(
        &self,
        check_deltas: bool,
        from: u64,
        mut visit: impl FnMut(Meta<'_>) -> Result<(), E>,
    ) -> Result<(), E> {
        let mut edits = self.meta.range(from..).peekable();
        let mut edited_group = None;
        let mut chunk_group = None;
        let mut chunk = Vec::new();
        self.transaction.visit_range(
            Table::NodeChunks,
            &from.to_be_bytes(),
            &u64::MAX.to_be_bytes(),
            |key, value| {
                let Some((group, part)) = meta::parse_key(key)? else {
                    return Ok(());
                };
                if edited_group == Some(group) {
                    return Ok(());
                }
                while let Some((edited, edit)) = edits.next_if(|(edited, _)| **edited < group) {
                    visit(Meta::Edited {
                        group: *edited,
                        edit: edit.view()?,
                    })?;
                }
                if let Some((_, edit)) = edits.next_if(|(edited, _)| **edited == group) {
                    edited_group = Some(group);
                    return visit(Meta::Edited {
                        group,
                        edit: edit.view()?,
                    });
                }
                match part {
                    Part::Chunk => {
                        chunk_group = Some(group);
                        if check_deltas {
                            chunk.clear();
                            chunk.extend_from_slice(value);
                        }
                        visit(Meta::Chunk {
                            group,
                            bytes: value,
                        })
                    }
                    Part::Delta(slot) if chunk_group == Some(group) => {
                        let delta = Delta::parse(key, value)?;
                        if check_deltas
                            && MetaBytes::trusted(&chunk)?.label_set(slot)? != delta.before
                        {
                            return Err(GraphError::Corrupt(
                                "a node meta delta does not amend its chunk",
                            )
                            .into());
                        }
                        visit(Meta::Delta { group, delta })
                    }
                    Part::Delta(_) => {
                        Err(GraphError::Corrupt("a node meta delta has no chunk").into())
                    }
                }
            },
        )?;
        for (group, edit) in edits {
            visit(Meta::Edited {
                group: *group,
                edit: edit.view()?,
            })?;
        }
        Ok(())
    }

    /// The label set of the node with ID `id`, or `None` when there is no such node.
    pub(crate) fn node_label_set(&self, id: NodeId) -> Result<Option<LabelSetId>, GraphError> {
        let (group, slot) = meta::locate(id);
        if let Some(edit) = self.meta.get(&group) {
            return edit.label_set(slot);
        }
        let key = meta::delta_key(group, slot);
        let delta = self
            .transaction
            .get_with(Table::NodeChunks, &key, |bytes| Delta::parse(&key, bytes))?;
        let chunk =
            self.transaction
                .get_with(Table::NodeChunks, &meta::meta_key(group), |bytes| {
                    MetaBytes::parse(group, bytes)?.label_set(slot)
                })?;
        match (chunk, delta) {
            (Some(state), None) => Ok(state),
            (Some(state), Some(delta)) if delta.before == state => Ok(delta.after),
            (Some(_), Some(_)) => Err(GraphError::Corrupt(
                "a node meta delta does not amend its chunk",
            )),
            (None, Some(_)) => Err(GraphError::Corrupt("a node meta delta has no chunk")),
            (None, None) => {
                let mut stray = false;
                self.transaction.visit_prefix(
                    Table::NodeChunks,
                    &meta::group_prefix(group),
                    |_, _| {
                        stray = true;
                        Ok::<(), GraphError>(())
                    },
                )?;
                if stray {
                    return Err(GraphError::Corrupt("a node meta delta has no chunk"));
                }
                Ok(None)
            }
        }
    }

    pub(crate) fn node_labels(
        &self,
        id: NodeId,
    ) -> Result<Option<Arc<[Token<Label>]>>, GraphError> {
        self.node_label_set(id)?
            .map(|set| self.label_set(set))
            .transpose()
    }

    /// Calls `read` with the labels of the node with ID `id`, if there is such a node, reading
    /// its group's node meta through `reader`.
    pub(crate) fn with_labels<R>(
        &self,
        reader: &mut LabelReader,
        id: NodeId,
        read: impl FnOnce(&[Token<Label>]) -> R,
    ) -> Result<Option<R>, GraphError> {
        let (group, slot) = meta::locate(id);
        if self.meta.contains_key(&group) {
            return Ok(self.node_labels(id)?.map(|labels| read(&labels)));
        }
        reader.with_labels(
            group,
            slot,
            || self.load_meta(group),
            || self.node_labels(id),
            read,
        )
    }

    fn stored_group(&self, group: u64) -> Result<Option<meta::StoredGroup>, GraphError> {
        let mut chunk: Option<Box<[u8]>> = None;
        let mut deltas = Vec::new();
        self.transaction.visit_prefix(
            Table::NodeChunks,
            &meta::group_prefix(group),
            |key, value| {
                match meta::parse_key(key)? {
                    Some((_, Part::Chunk)) => {
                        MetaBytes::parse(group, value)?;
                        chunk = Some(value.into());
                    }
                    Some((_, Part::Delta(_))) => {
                        let delta = Delta::parse(key, value)?;
                        let Some(bytes) = &chunk else {
                            return Err(GraphError::Corrupt("a node meta delta has no chunk"));
                        };
                        if MetaBytes::trusted(bytes)?.label_set(delta.slot)? != delta.before {
                            return Err(GraphError::Corrupt(
                                "a node meta delta does not amend its chunk",
                            ));
                        }
                        deltas.push(delta);
                    }
                    None => {}
                }
                Ok::<(), GraphError>(())
            },
        )?;
        Ok(chunk.map(|chunk| (chunk, deltas)))
    }

    fn load_meta(&self, group: u64) -> Result<Option<meta::Loaded>, GraphError> {
        let Some((bytes, deltas)) = self.stored_group(group)? else {
            return Ok(None);
        };
        let sets = MetaBytes::trusted(&bytes)?
            .head()
            .dictionary()
            .map(|set| self.label_set(set))
            .collect::<Result<_, _>>()?;
        let deltas = deltas
            .into_iter()
            .map(|delta| {
                let labels = delta.after.map(|set| self.label_set(set)).transpose()?;
                Ok((delta.slot, labels))
            })
            .collect::<Result<_, GraphError>>()?;
        Ok(Some((bytes, sets, deltas)))
    }

    fn carries(&self, state: Slot, label: Token<Label>) -> Result<bool, GraphError> {
        match state {
            None => Ok(false),
            Some(set) => Ok(self.label_set(set)?.binary_search(&label).is_ok()),
        }
    }

    pub(crate) fn node_ids(&self) -> Result<Vec<NodeId>, GraphError> {
        self.collect_node_ids(None)
    }

    /// The nodes that carry `label`, in ascending ID order.
    pub(crate) fn node_ids_with_label(
        &self,
        label: Token<Label>,
    ) -> Result<Vec<NodeId>, GraphError> {
        self.collect_node_ids(Some(label))
    }

    fn collect_node_ids(&self, label: Option<Token<Label>>) -> Result<Vec<NodeId>, GraphError> {
        let mut ids = Vec::new();
        self.visit_node_groups(0, label, |_, group| {
            ids.extend_from_slice(group);
            Ok::<_, GraphError>(true)
        })?;
        Ok(ids)
    }

    /// The first row group from `from` on that holds a live node, one that carries `label` when
    /// it is given, with those nodes in ascending ID order.
    pub(crate) fn node_group(
        &self,
        from: u64,
        label: Option<Token<Label>>,
    ) -> Result<Option<(u64, Vec<NodeId>)>, GraphError> {
        let mut found = None;
        self.visit_node_groups(from, label, |group, ids| {
            found = Some((group, ids.to_vec()));
            Ok::<_, GraphError>(false)
        })?;
        Ok(found)
    }

    /// Calls `visit` with each row group from `from` on that holds a live node, one that
    /// carries `label` when it is given, and those nodes in ascending ID order, until it
    /// returns false.
    pub(crate) fn visit_node_groups<E: From<GraphError>>(
        &self,
        from: u64,
        label: Option<Token<Label>>,
        mut visit: impl FnMut(u64, &[NodeId]) -> Result<bool, E>,
    ) -> Result<(), E> {
        let mut ids = Vec::new();
        let mut current = None;
        let visited = self.visit_meta_from(true, from, |entry| {
            let at = match &entry {
                Meta::Chunk { group, .. }
                | Meta::Delta { group, .. }
                | Meta::Edited { group, .. } => *group,
            };
            if current != Some(at) {
                if let Some(done) = current
                    && !ids.is_empty()
                    && !visit(done, &ids).map_err(Flow::Failed)?
                {
                    return Err(Flow::Stop);
                }
                ids.clear();
                current = Some(at);
            }
            match label {
                Some(label) => self.labelled_entry(entry, label, &mut ids),
                None => live_entry(entry, &mut ids),
            }
            .map_err(|error| Flow::Failed(error.into()))
        });
        match visited {
            Ok(()) => {}
            Err(Flow::Stop) => return Ok(()),
            Err(Flow::Failed(error)) => return Err(error),
        }
        if let Some(done) = current
            && !ids.is_empty()
        {
            visit(done, &ids)?;
        }
        Ok(())
    }

    fn labelled_entry(
        &self,
        entry: Meta<'_>,
        label: Token<Label>,
        ids: &mut Vec<NodeId>,
    ) -> Result<(), GraphError> {
        match entry {
            Meta::Chunk { group, bytes } => {
                let stored = MetaBytes::parse(group, bytes)?;
                if stored.head().label_count(label) == 0 {
                    return Ok(());
                }
                let carries = stored
                    .head()
                    .dictionary()
                    .map(|set| self.carries(Some(set), label))
                    .collect::<Result<Vec<bool>, GraphError>>()?;
                if let [true] = carries.as_slice() {
                    ids.reserve(stored.head().live() as usize);
                    for (index, word) in stored.alive_words().into_iter().enumerate() {
                        let mut rest = word;
                        while rest != 0 {
                            let slot = index * 64 + rest.trailing_zeros() as usize;
                            rest &= rest - 1;
                            ids.push(meta::node_at(group, slot));
                        }
                    }
                    return Ok(());
                }
                for (slot, code) in stored.live_codes() {
                    match carries.get(code) {
                        Some(true) => ids.push(meta::node_at(group, slot)),
                        Some(false) => {}
                        None => {
                            return Err(GraphError::Corrupt(
                                "a node meta code is outside its dictionary",
                            ));
                        }
                    }
                }
            }
            Meta::Delta { group, delta } => amend(
                ids,
                meta::node_at(group, delta.slot),
                self.carries(delta.before, label)?,
                self.carries(delta.after, label)?,
            )?,
            Meta::Edited { group, edit } => {
                let mut carrying = BTreeSet::new();
                for (set, _) in edit.uses() {
                    if self.carries(Some(set), label)? {
                        carrying.insert(set);
                    }
                }
                if carrying.is_empty() {
                    return Ok(());
                }
                for (slot, set) in edit.live() {
                    if carrying.contains(&set) {
                        ids.push(meta::node_at(group, slot));
                    }
                }
            }
        }
        Ok(())
    }

    /// Whether a row group that holds IDs from `node` on keeps node meta. When none does, no node
    /// has an ID of `node` or above.
    pub(crate) fn may_hold_from(&self, node: NodeId) -> Result<bool, GraphError> {
        match self.visit_meta_from(false, row_group(node), |_| Err(Flow::<GraphError>::Stop)) {
            Ok(()) => Ok(false),
            Err(Flow::Stop) => Ok(true),
            Err(Flow::Failed(error)) => Err(error),
        }
    }

    /// How many nodes there are, from the live count each group keeps. Like a label count, it
    /// checks only the head of each chunk and trusts each delta's state in the chunk.
    pub(crate) fn count_nodes(&self) -> Result<u64, GraphError> {
        let mut count = 0u64;
        self.visit_meta(false, |entry| {
            count = match entry {
                Meta::Chunk { group, bytes } => count + MetaHead::parse(group, bytes)?.live(),
                Meta::Delta { delta, .. } => (count + u64::from(delta.after.is_some()))
                    .checked_sub(u64::from(delta.before.is_some()))
                    .ok_or(GraphError::Corrupt(
                        "a node meta delta does not amend its chunk",
                    ))?,
                Meta::Edited { edit, .. } => count + edit.live().count() as u64,
            };
            Ok(())
        })?;
        Ok(count)
    }

    /// How many nodes carry `label`, from the label counts each group keeps. A count checks only
    /// the head of each chunk and trusts each delta's state in the chunk.
    pub(crate) fn count_nodes_with_label(&self, label: Token<Label>) -> Result<u64, GraphError> {
        let mut count = 0u64;
        self.visit_meta(false, |entry| {
            count = match entry {
                Meta::Chunk { group, bytes } => {
                    count + MetaHead::parse(group, bytes)?.label_count(label)
                }
                Meta::Delta { delta, .. } => (count + u64::from(self.carries(delta.after, label)?))
                    .checked_sub(u64::from(self.carries(delta.before, label)?))
                    .ok_or(GraphError::Corrupt(
                        "a node meta delta does not amend its chunk",
                    ))?,
                Meta::Edited { edit, .. } => {
                    let mut carrying = count;
                    for (set, uses) in edit.uses() {
                        if self.carries(Some(set), label)? {
                            carrying += u64::from(uses);
                        }
                    }
                    carrying
                }
            };
            Ok(())
        })?;
        Ok(count)
    }

    /// Fails with `NodeHasRelationships` while any relationship still touches the node.
    pub(crate) fn ensure_detached(&self, node: NodeId) -> Result<(), GraphError> {
        if self.adjacent(node, Direction::Both, None)?.is_empty() {
            Ok(())
        } else {
            Err(GraphError::NodeHasRelationships(node))
        }
    }

    /// The relationships touching `node`, optionally of one type, outgoing ones first, each
    /// side by type and then by ID. A self-loop is reported once under `Both`.
    pub(crate) fn adjacent(
        &self,
        node: NodeId,
        direction: Direction,
        rel_type: Option<Token<RelationshipType>>,
    ) -> Result<Vec<Adjacent>, GraphError> {
        self.adjacent_with(&mut AdjacencyReader::default(), node, direction, rel_type)
    }

    /// [`adjacent`](Self::adjacent) through `reader`, which holds the segments a scan of
    /// consecutive nodes reads again.
    pub(crate) fn adjacent_with(
        &self,
        reader: &mut AdjacencyReader,
        node: NodeId,
        direction: Direction,
        rel_type: Option<Token<RelationshipType>>,
    ) -> Result<Vec<Adjacent>, GraphError> {
        let mut found = Vec::new();
        self.adjacent_into(reader, node, direction, rel_type, &mut found)?;
        Ok(found)
    }

    /// Appends to `found` what [`GraphTx::adjacent_with`] returns.
    pub(crate) fn adjacent_into(
        &self,
        reader: &mut AdjacencyReader,
        node: NodeId,
        direction: Direction,
        rel_type: Option<Token<RelationshipType>>,
        found: &mut Vec<Adjacent>,
    ) -> Result<(), GraphError> {
        if direction != Direction::Incoming {
            self.visit_adjacency(reader, Side::Outgoing, node, rel_type, &mut |adjacent| {
                found.push(adjacent);
            })?;
        }
        if direction != Direction::Outgoing {
            let skip_self_loops = direction == Direction::Both;
            self.visit_adjacency(reader, Side::Incoming, node, rel_type, &mut |adjacent| {
                if !(skip_self_loops && adjacent.other == node) {
                    found.push(adjacent);
                }
            })?;
        }
        Ok(())
    }

    fn visit_adjacency_prefix(
        &self,
        reader: &mut AdjacencyReader,
        side: Side,
        (prefix, position): (&[u8], usize),
        mut visit: impl FnMut(
            (&[u8], SegmentId, Entry),
            &[u8],
            &mut Option<adjacency::Checked>,
        ) -> Result<(), GraphError>,
    ) -> Result<(), GraphError> {
        let entries = reader.entries(side, prefix, |window| {
            let mut held = Vec::new();
            self.transaction.visit_range_peek(
                Table::Adjacency,
                window.start(),
                &window.end(),
                |key, value| {
                    if held
                        .last()
                        .is_some_and(|(last, _, _): &adjacency::Verified| **last >= *key)
                    {
                        return Err(ENTRIES_OUT_OF_ORDER);
                    }
                    if window.holds(key) {
                        held.push((key.into(), Lazy::of(value), None));
                    }
                    Ok::<(), GraphError>(())
                },
            )?;
            Ok::<_, GraphError>(held)
        })?;
        match entries {
            Entries::Held(entries) => {
                for (key, value, verified) in entries {
                    let (id, entry) = adjacency::parse_key(key)?;
                    if !entry.covers(position) {
                        visit((key, id, entry), &[], verified)?;
                        continue;
                    }
                    if let Lazy::Apart(_) = value {
                        *value = Lazy::Bytes(fetch_adjacency(&self.transaction, key)?);
                    }
                    if let Lazy::Bytes(bytes) = value {
                        visit((key, id, entry), bytes, verified)?;
                    }
                }
                Ok(())
            }
            Entries::Stored => {
                self.transaction
                    .visit_prefix_peek(Table::Adjacency, prefix, |key, value| {
                        let (id, entry) = adjacency::parse_key(key)?;
                        match value {
                            _ if !entry.covers(position) => visit((key, id, entry), &[], &mut None),
                            Peek::Bytes(bytes) => visit((key, id, entry), bytes, &mut None),
                            Peek::Apart(_) => {
                                let bytes = fetch_adjacency(&self.transaction, key)?;
                                visit((key, id, entry), &bytes, &mut None)
                            }
                        }
                    })
            }
        }
    }

    fn visit_adjacency(
        &self,
        reader: &mut AdjacencyReader,
        side: Side,
        node: NodeId,
        rel_type: Option<Token<RelationshipType>>,
        emit: &mut impl FnMut(Adjacent),
    ) -> Result<(), GraphError> {
        let (first, position) = adjacency::locate(side, node, 0);
        if let Some(rel_type) = rel_type {
            let id = SegmentId {
                rel_type: rel_type.id(),
                ..first
            };
            return self.visit_typed_adjacency(reader, &id, position, emit);
        }
        let last = SegmentId {
            rel_type: u32::MAX,
            ..first
        };
        let fetch = |key: &[u8]| fetch_adjacency(&self.transaction, key);
        let mut edits = self.adjacency.range(first..=last).peekable();
        let mut current: Option<(u32, SlotRead)> = None;
        let mut edited = None;
        self.visit_adjacency_prefix(
            reader,
            side,
            (
                &adjacency::segment_prefix(side, first.group, first.segment),
                position,
            ),
            |(key, id, entry), value, verified| {
                if edited == Some(id.rel_type) {
                    return Ok(());
                }
                if current
                    .as_ref()
                    .is_none_or(|(held, _)| *held != id.rel_type)
                {
                    if let Some((held, read)) = current.take() {
                        read.finish(&mut typed(held, emit))?;
                    }
                    while let Some((edit_id, edit)) =
                        edits.next_if(|(edit_id, _)| edit_id.rel_type <= id.rel_type)
                    {
                        edit.visit(position, &mut typed(edit_id.rel_type, emit), &fetch)?;
                        if edit_id.rel_type == id.rel_type {
                            edited = Some(id.rel_type);
                            return Ok(());
                        }
                    }
                    current = Some((id.rel_type, SlotRead::default()));
                }
                match &mut current {
                    Some((held, read)) => read.visit(
                        entry,
                        position,
                        (key, value, verified),
                        &mut typed(*held, emit),
                    ),
                    None => Ok(()),
                }
            },
        )?;
        if let Some((held, read)) = current {
            read.finish(&mut typed(held, emit))?;
        }
        for (edit_id, edit) in edits {
            edit.visit(position, &mut typed(edit_id.rel_type, emit), &fetch)?;
        }
        Ok(())
    }

    fn visit_typed_adjacency(
        &self,
        reader: &mut AdjacencyReader,
        id: &SegmentId,
        position: usize,
        emit: &mut impl FnMut(Adjacent),
    ) -> Result<(), GraphError> {
        let mut emit = typed(id.rel_type, emit);
        if let Some(edit) = self.adjacency.get(id) {
            return edit.visit(position, &mut emit, &|key| {
                fetch_adjacency(&self.transaction, key)
            });
        }
        let mut read = SlotRead::default();
        self.visit_adjacency_prefix(
            reader,
            id.side,
            (&id.prefix(), position),
            |(key, _, entry), value, verified| {
                read.visit(entry, position, (key, value, verified), &mut emit)
            },
        )?;
        read.finish(&mut emit)
    }

    /// Looks a name up without creating it, so a read never writes.
    pub(crate) fn lookup_token<K: TokenKind>(
        &self,
        name: &str,
    ) -> Result<Option<Token<K>>, GraphError> {
        if let Some(token) = self.tokens.get(name).or_else(|| self.own_tokens.get(name)) {
            return Ok(Some(token));
        }
        let bytes = self
            .transaction
            .get(Table::TokenByName, &keys::token_name_key(K::TAG, name))?;
        let token = bytes
            .map(|bytes| decode_u32(&bytes).map(Token::new))
            .transpose()?;
        if let Some(token) = token {
            self.remember_token(Arc::from(name), token);
        }
        Ok(token)
    }

    pub(crate) fn token_name<K: TokenKind>(&self, token: Token<K>) -> Result<Arc<str>, GraphError> {
        if let Some(name) = self
            .tokens
            .name(token)
            .or_else(|| self.own_tokens.name(token))
        {
            return Ok(name);
        }
        let bytes = self
            .transaction
            .get(Table::TokenById, &keys::token_id_key(K::TAG, token.id()))?
            .ok_or(GraphError::Corrupt("token has no name"))?;
        let name: Arc<str> = String::from_utf8(bytes)
            .map_err(|_| GraphError::Decode(DecodeError::InvalidUtf8))?
            .into();
        self.remember_token(Arc::clone(&name), token);
        Ok(name)
    }

    fn remember_token<K: TokenKind>(&self, name: Arc<str>, token: Token<K>) {
        if self.reads_committed_tokens {
            self.tokens.insert(name, token);
        } else {
            self.own_tokens.insert(name, token);
        }
    }
}

impl<T: WriteTransaction> GraphTx<T> {
    pub(crate) fn commit(mut self) -> Result<(), GraphError> {
        self.flush()?;
        let definitions = match &self.catalog {
            Some(Catalog {
                shared,
                changed: true,
            }) => Some((shared, self.index_definitions()?)),
            _ => None,
        };
        let publishing = definitions.as_ref().map(|(shared, _)| {
            let publishing = lock(&shared.publishing);
            shared.changes.fetch_add(1, Ordering::SeqCst);
            publishing
        });
        let mut counts = self.column_deltas.as_ref().map(|shared| lock(shared));
        let committed = self.transaction.commit();
        if let Some(counts) = &mut counts {
            if committed.is_ok() {
                counts.extend(std::mem::take(&mut self.own_column_deltas));
            } else {
                counts.clear();
            }
        }
        if let Some((shared, definitions)) = definitions {
            if committed.is_ok() {
                *lock(&shared.definitions) = definitions;
            }
            shared.changes.fetch_add(1, Ordering::SeqCst);
        }
        drop(publishing);
        Ok(committed?)
    }

    pub(crate) fn rollback(self) -> Result<(), GraphError> {
        Ok(self.transaction.rollback()?)
    }

    fn flush(&mut self) -> Result<(), GraphError> {
        for (set, (key, users)) in std::mem::take(&mut self.new_label_sets) {
            if users > 0 {
                self.store_label_set(set, &key)?;
            }
        }
        for (key, next) in std::mem::take(&mut self.counters) {
            self.transaction
                .put(Table::Metadata, &key, &next.to_be_bytes())?;
        }
        self.flush_postings()?;
        self.flush_columns()?;
        self.flush_adjacency()?;
        self.flush_relationships()?;
        for (group, edit) in std::mem::take(&mut self.meta) {
            match edit.flush()? {
                Flush::Rewrite { view, stale } => {
                    let key = meta::meta_key(group);
                    match view.encode(group, |set| self.label_set(set))? {
                        Some(chunk) => self.transaction.put(Table::NodeChunks, &key, &chunk)?,
                        None => self.transaction.delete(Table::NodeChunks, &key)?,
                    }
                    for slot in stale {
                        self.transaction
                            .delete(Table::NodeChunks, &meta::delta_key(group, slot))?;
                    }
                }
                Flush::Deltas { put, delete } => {
                    for delta in put {
                        self.transaction.put(
                            Table::NodeChunks,
                            &meta::delta_key(group, delta.slot),
                            &delta.encode(group),
                        )?;
                    }
                    for slot in delete {
                        self.transaction
                            .delete(Table::NodeChunks, &meta::delta_key(group, slot))?;
                    }
                }
            }
        }
        Ok(())
    }

    fn flush_adjacency(&mut self) -> Result<(), GraphError> {
        for (id, edit) in std::mem::take(&mut self.adjacency) {
            let flush = edit.flush(&id, &|key| fetch_adjacency(&self.transaction, key))?;
            for key in flush.delete {
                self.transaction.delete(Table::Adjacency, &key)?;
            }
            for (key, bytes) in flush.put {
                self.transaction.put(Table::Adjacency, &key, &bytes)?;
            }
        }
        Ok(())
    }

    fn flush_relationships(&mut self) -> Result<(), GraphError> {
        for (chunk, edit) in std::mem::take(&mut self.relationship_chunks) {
            let key = relationships::chunk_key(chunk);
            match edit.encode() {
                Some(bytes) => self
                    .transaction
                    .put(Table::RelationshipChunks, &key, &bytes)?,
                None => self.transaction.delete(Table::RelationshipChunks, &key)?,
            }
        }
        self.relationship_bytes = 0;
        Ok(())
    }

    fn store_relationship(
        &mut self,
        id: RelationshipId,
        record: Option<&RelationshipRecord>,
    ) -> Result<(), GraphError> {
        let (chunk, slot) = relationships::locate(id);
        let edit = match self.relationship_chunks.entry(chunk) {
            btree_map::Entry::Occupied(held) => held.into_mut(),
            btree_map::Entry::Vacant(vacant) => {
                let edit = self
                    .transaction
                    .get_with(
                        Table::RelationshipChunks,
                        &relationships::chunk_key(chunk),
                        ChunkEdit::load,
                    )?
                    .unwrap_or_default();
                self.relationship_bytes += edit.bytes();
                vacant.insert(edit)
            }
        };
        let before = edit.bytes();
        let encoded = record.map(|record| {
            self.record_buffer.clear();
            record.encode_into(&mut self.record_buffer);
            self.record_buffer.as_slice()
        });
        edit.set(slot, encoded);
        self.relationship_bytes = self.relationship_bytes - before + edit.bytes();
        if self.relationship_bytes > HELD_RELATIONSHIP_BYTES {
            self.flush_relationships()?;
        }
        Ok(())
    }

    fn move_relationships_to_chunks(&mut self) -> Result<(), GraphError> {
        const BATCH_SHIFT: u32 = 16;
        let mut batches = BTreeSet::new();
        self.transaction
            .visit_prefix(Table::Relationships, &[], |key, _| {
                batches.insert(decode_u64(key)? >> BATCH_SHIFT);
                Ok::<(), GraphError>(())
            })?;
        for batch in batches {
            let start = keys::element_key(batch << BATCH_SHIFT);
            let end = batch
                .checked_add(1)
                .and_then(|next| next.checked_mul(1 << BATCH_SHIFT))
                .map_or([0xff; 9].to_vec(), |next| keys::element_key(next).to_vec());
            let mut chunks: BTreeMap<u64, ChunkEdit> = BTreeMap::new();
            self.transaction
                .visit_range(Table::Relationships, &start, &end, |key, value| {
                    let record = RelationshipRecord::decode(value)?;
                    let (chunk, slot) = relationships::locate(RelationshipId(decode_u64(key)?));
                    chunks
                        .entry(chunk)
                        .or_default()
                        .set(slot, Some(&record.encode()));
                    Ok::<(), GraphError>(())
                })?;
            for (chunk, edit) in chunks {
                if let Some(bytes) = edit.encode() {
                    self.transaction.put(
                        Table::RelationshipChunks,
                        &relationships::chunk_key(chunk),
                        &bytes,
                    )?;
                }
            }
            self.transaction
                .delete_range(Table::Relationships, &start, &end)?;
        }
        Ok(())
    }

    fn meta_edit(&mut self, group: u64) -> Result<&mut GroupEdit, GraphError> {
        if !self.meta.contains_key(&group) {
            let edit = match self.stored_group(group)? {
                Some((chunk, deltas)) => GroupEdit::load(Some(chunk), deltas)?,
                None => GroupEdit::load(None, Vec::new())?,
            };
            self.meta.insert(group, edit);
        }
        self.meta
            .get_mut(&group)
            .ok_or(GraphError::Corrupt("a node meta edit vanished"))
    }

    fn intern_label_set(
        &mut self,
        labels: &BTreeSet<Token<Label>>,
    ) -> Result<LabelSetId, GraphError> {
        if labels.is_empty() {
            return Ok(LabelSetId::EMPTY);
        }
        let labels: Arc<[Token<Label>]> = labels.iter().copied().collect();
        let key = meta::label_set_key(&labels);
        if let Some(set) = self
            .label_sets
            .id(&key)
            .or_else(|| self.own_label_sets.id(&key))
        {
            return Ok(set);
        }
        let stored = self
            .transaction
            .get(Table::LabelSetByTokens, &key)?
            .map(|bytes| decode_u32(&bytes))
            .transpose()?;
        let set = match stored {
            Some(id) => LabelSetId(id),
            None => {
                let mut id = self.next_counter(keys::NEXT_LABEL_SET_KEY)?;
                if id == u64::from(LabelSetId::EMPTY.0) {
                    id = self.next_counter(keys::NEXT_LABEL_SET_KEY)?;
                }
                let id = u32::try_from(id).map_err(|_| GraphError::IdSpaceExhausted)?;
                let set = LabelSetId(id);
                self.new_label_sets.insert(set, (key, 0));
                set
            }
        };
        self.own_label_sets.insert(set, labels);
        Ok(set)
    }

    fn set_node_label_set(
        &mut self,
        group: u64,
        slot: usize,
        state: Option<LabelSetId>,
    ) -> Result<(), GraphError> {
        let edit = self.meta_edit(group)?;
        let was = edit.label_set(slot)?;
        edit.set(slot, state)?;
        if let Some(set) = state
            && let Some((_, users)) = self.new_label_sets.get_mut(&set)
        {
            *users += 1;
        }
        if let Some(set) = was
            && let Some((_, users)) = self.new_label_sets.get_mut(&set)
        {
            *users -= 1;
            if *users == 0 {
                self.new_label_sets.remove(&set);
                self.own_label_sets.remove(set);
            }
        }
        Ok(())
    }

    fn store_label_set(&mut self, set: LabelSetId, key: &[u8]) -> Result<(), GraphError> {
        self.transaction
            .put(Table::LabelSets, &set.0.to_be_bytes(), key)?;
        self.transaction
            .put(Table::LabelSetByTokens, key, &set.0.to_be_bytes())?;
        Ok(())
    }

    pub(crate) fn intern_token<K: TokenKind>(
        &mut self,
        name: &str,
    ) -> Result<Token<K>, GraphError> {
        if let Some(token) = self.lookup_token(name)? {
            return Ok(token);
        }
        let id = u32::try_from(self.next_counter(&keys::next_token_key(K::TAG))?)
            .map_err(|_| GraphError::IdSpaceExhausted)?;
        self.transaction.put(
            Table::TokenByName,
            &keys::token_name_key(K::TAG, name),
            &id.to_be_bytes(),
        )?;
        self.transaction.put(
            Table::TokenById,
            &keys::token_id_key(K::TAG, id),
            name.as_bytes(),
        )?;
        let token = Token::new(id);
        self.own_tokens.insert(Arc::from(name), token);
        Ok(token)
    }

    pub(crate) fn create_node(&mut self, record: NodeRecord) -> Result<NodeId, GraphError> {
        let id = NodeId(self.next_counter(keys::NEXT_NODE_ID_KEY)?);
        let set = self.intern_label_set(&record.labels)?;
        let (group, slot) = meta::locate(id);
        self.set_node_label_set(group, slot, Some(set))?;
        self.reindex(id.0, None, Some(Element::Node(&record)))?;
        self.create_column_node(group, slot, record.properties)?;
        Ok(id)
    }

    pub(crate) fn create_relationship(
        &mut self,
        record: &RelationshipRecord,
    ) -> Result<RelationshipId, GraphError> {
        self.require_node(record.source)?;
        self.require_node(record.target)?;
        let id = RelationshipId(self.next_counter(keys::NEXT_RELATIONSHIP_ID_KEY)?);
        self.store_relationship(id, Some(record))?;
        for (side, node, other) in [
            (Side::Outgoing, record.source, record.target),
            (Side::Incoming, record.target, record.source),
        ] {
            let (segment, position) = adjacency::locate(side, node, record.rel_type.id());
            adjacency_edit(&mut self.adjacency, &self.transaction, segment)?.add(
                position,
                Edge {
                    relationship: id,
                    other,
                },
            );
        }
        self.reindex(id.0, None, Some(Element::Relationship(record)))?;
        Ok(id)
    }

    pub(crate) fn set_node_property(
        &mut self,
        node: NodeId,
        key: Token<PropertyKey>,
        value: PropertyValue,
    ) -> Result<(), GraphError> {
        self.write_node_property(node, key, Some(value))
    }

    pub(crate) fn remove_node_property(
        &mut self,
        node: NodeId,
        key: Token<PropertyKey>,
    ) -> Result<(), GraphError> {
        self.write_node_property(node, key, None)
    }

    pub(crate) fn set_relationship_property(
        &mut self,
        relationship: RelationshipId,
        key: Token<PropertyKey>,
        value: PropertyValue,
    ) -> Result<(), GraphError> {
        self.edit_relationship(relationship, |record| {
            record.properties.insert(key, value);
        })
    }

    pub(crate) fn remove_relationship_property(
        &mut self,
        relationship: RelationshipId,
        key: Token<PropertyKey>,
    ) -> Result<(), GraphError> {
        self.edit_relationship(relationship, |record| {
            record.properties.remove(&key);
        })
    }

    /// Sets each property of `changes` on the relationship, or removes it with `None`, in one
    /// write of its record.
    pub(crate) fn set_relationship_properties(
        &mut self,
        relationship: RelationshipId,
        changes: Vec<(Token<PropertyKey>, Option<PropertyValue>)>,
    ) -> Result<(), GraphError> {
        self.edit_relationship(relationship, |record| {
            for (key, value) in changes {
                match value {
                    Some(value) => record.properties.insert(key, value),
                    None => record.properties.remove(&key),
                };
            }
        })
    }

    #[cfg(test)]
    pub(crate) fn add_label(
        &mut self,
        node: NodeId,
        label: Token<Label>,
    ) -> Result<(), GraphError> {
        self.relabel(node, &[label], true)
    }

    /// Adds every label of `labels` to the node, interning only the label set it ends with.
    pub(crate) fn add_labels(
        &mut self,
        node: NodeId,
        labels: &[Token<Label>],
    ) -> Result<(), GraphError> {
        self.relabel(node, labels, true)
    }

    /// Removes every label of `labels` from the node, interning only the label set it ends with.
    pub(crate) fn remove_labels(
        &mut self,
        node: NodeId,
        labels: &[Token<Label>],
    ) -> Result<(), GraphError> {
        self.relabel(node, labels, false)
    }

    #[cfg(test)]
    pub(crate) fn remove_label(
        &mut self,
        node: NodeId,
        label: Token<Label>,
    ) -> Result<(), GraphError> {
        self.relabel(node, &[label], false)
    }

    /// Removes the relationship and returns its record, or returns `None` when it does not exist.
    pub(crate) fn delete_relationship(
        &mut self,
        id: RelationshipId,
    ) -> Result<Option<RelationshipRecord>, GraphError> {
        let Some(record) = self.relationship(id)? else {
            return Ok(None);
        };
        self.store_relationship(id, None)?;
        for (side, node) in [
            (Side::Outgoing, record.source),
            (Side::Incoming, record.target),
        ] {
            let (segment, position) = adjacency::locate(side, node, record.rel_type.id());
            adjacency_edit(&mut self.adjacency, &self.transaction, segment)?.remove(
                position,
                id,
                &|key| fetch_adjacency(&self.transaction, key),
            )?;
        }
        self.reindex(id.0, Some(Element::Relationship(&record)), None)?;
        Ok(Some(record))
    }

    /// Removes the node with its label and property index entries and returns its record, or
    /// returns `None` when it does not exist. Relationships that touch it are left alone, so the
    /// caller must delete them or reject the transaction, which `ensure_detached` checks.
    pub(crate) fn delete_node(&mut self, id: NodeId) -> Result<Option<NodeRecord>, GraphError> {
        let Some(labels) = self.node_labels(id)? else {
            return Ok(None);
        };
        let properties = self.with_reader(|tx, reader| tx.live_node_properties(reader, id))?;
        let record = NodeRecord {
            labels: labels.iter().copied().collect(),
            properties,
        };
        self.reindex(id.0, Some(Element::Node(&record)), None)?;
        let (group, slot) = meta::locate(id);
        for key in record.properties.keys() {
            self.set_column_value((group, *key), slot, None)?;
        }
        self.set_node_label_set(group, slot, None)?;
        Ok(Some(record))
    }

    fn require_node(&mut self, id: NodeId) -> Result<(), GraphError> {
        let mut reader = std::mem::take(&mut self.labels);
        let found = self.with_labels(&mut reader, id, |_| ());
        self.labels = reader;
        found?.ok_or(GraphError::NodeNotFound(id))
    }

    fn partial_record(
        &mut self,
        id: NodeId,
        labels: &[Token<Label>],
        keys: &BTreeSet<Token<PropertyKey>>,
    ) -> Result<NodeRecord, GraphError> {
        let (group, slot) = meta::locate(id);
        let mut properties = Properties::new();
        for key in keys {
            if let Some(value) =
                self.with_reader(|tx, reader| tx.column_value(reader, group, slot, *key))?
            {
                properties.insert(*key, value);
            }
        }
        Ok(NodeRecord {
            labels: labels.iter().copied().collect(),
            properties,
        })
    }

    fn read_labels(&mut self, id: NodeId) -> Result<Option<Arc<[Token<Label>]>>, GraphError> {
        let mut reader = std::mem::take(&mut self.labels);
        let labels = self.with_labels(&mut reader, id, |labels| Arc::from(labels));
        self.labels = reader;
        labels
    }

    fn write_node_property(
        &mut self,
        id: NodeId,
        key: Token<PropertyKey>,
        value: Option<PropertyValue>,
    ) -> Result<(), GraphError> {
        let labels = self.read_labels(id)?.ok_or(GraphError::NodeNotFound(id))?;
        let indexed =
            self.node_index_keys(|label, keys| labels.contains(&label) && keys.contains(&key))?;
        let (group, slot) = meta::locate(id);
        if !indexed.is_empty() {
            let before = self.partial_record(id, &labels, &indexed)?;
            let mut after = before.clone();
            match &value {
                Some(value) => after.properties.insert(key, value.clone()),
                None => after.properties.remove(&key),
            };
            self.reindex(
                id.0,
                Some(Element::Node(&before)),
                Some(Element::Node(&after)),
            )?;
        } else if value.is_none()
            && self
                .with_reader(|tx, reader| tx.column_value(reader, group, slot, key))?
                .is_none()
        {
            return Ok(());
        }
        self.set_column_value((group, key), slot, value)
    }

    fn relabel(
        &mut self,
        id: NodeId,
        labels: &[Token<Label>],
        add: bool,
    ) -> Result<(), GraphError> {
        let current = self.read_labels(id)?.ok_or(GraphError::NodeNotFound(id))?;
        let changed: BTreeSet<Token<Label>> = labels
            .iter()
            .copied()
            .filter(|label| current.binary_search(label).is_ok() != add)
            .collect();
        if changed.is_empty() {
            return Ok(());
        }
        let indexed = self.node_index_keys(|covered, _| changed.contains(&covered))?;
        let before = self.partial_record(id, &current, &indexed)?;
        let mut after = before.clone();
        for label in &changed {
            if add {
                after.labels.insert(*label);
            } else {
                after.labels.remove(label);
            }
        }
        let set = self.intern_label_set(&after.labels)?;
        let (group, slot) = meta::locate(id);
        self.set_node_label_set(group, slot, Some(set))?;
        if !indexed.is_empty() {
            self.reindex(
                id.0,
                Some(Element::Node(&before)),
                Some(Element::Node(&after)),
            )?;
        }
        Ok(())
    }

    fn edit_relationship<R>(
        &mut self,
        id: RelationshipId,
        edit: impl FnOnce(&mut RelationshipRecord) -> R,
    ) -> Result<R, GraphError> {
        let before = self
            .relationship(id)?
            .ok_or(GraphError::RelationshipNotFound(id))?;
        let mut record = before.clone();
        let result = edit(&mut record);
        self.store_relationship(id, Some(&record))?;
        self.reindex(
            id.0,
            Some(Element::Relationship(&before)),
            Some(Element::Relationship(&record)),
        )?;
        Ok(result)
    }

    fn next_counter(&mut self, key: &[u8]) -> Result<u64, GraphError> {
        let index = match self.counters.iter().position(|(held, _)| held == key) {
            Some(index) => index,
            None => {
                let stored = match self.transaction.get(Table::Metadata, key)? {
                    Some(bytes) => decode_u64(&bytes)?,
                    None => 0,
                };
                self.counters.push((key.to_vec(), stored));
                self.counters.len() - 1
            }
        };
        let held = &mut self.counters[index].1;
        let current = *held;
        *held = current.checked_add(1).ok_or(GraphError::IdSpaceExhausted)?;
        Ok(current)
    }
}

#[cfg(test)]
#[path = "graph.test.rs"]
mod tests;

#[cfg(test)]
#[path = "graph/crash.test.rs"]
mod crash_tests;

#[cfg(test)]
#[path = "graph/maintenance.test.rs"]
mod maintenance_tests;

#[cfg(test)]
#[path = "graph/node_meta.test.rs"]
mod node_meta_tests;

#[cfg(test)]
#[path = "graph/node_meta_crash.test.rs"]
mod node_meta_crash_tests;

#[cfg(test)]
#[path = "graph/node_meta_corrupt.test.rs"]
mod node_meta_corrupt_tests;

#[cfg(test)]
#[path = "graph/columns_model.test.rs"]
mod columns_model_tests;

#[cfg(test)]
#[path = "graph/columns_crash.test.rs"]
mod columns_crash_tests;

#[cfg(test)]
#[path = "graph/columns_corrupt.test.rs"]
mod columns_corrupt_tests;

#[cfg(test)]
#[path = "graph/adjacency_model.test.rs"]
mod adjacency_model_tests;

#[cfg(test)]
#[path = "graph/adjacency_crash.test.rs"]
mod adjacency_crash_tests;

fn visit_held(
    chunk: u64,
    edit: &ChunkEdit,
    visit: &mut impl FnMut(RelationshipId, &[u8]) -> Result<(), GraphError>,
) -> Result<(), GraphError> {
    edit.entries()
        .try_for_each(|(slot, record)| visit(relationships::relationship_at(chunk, slot), record))
}

fn fetch_adjacency(
    transaction: &impl ReadTransaction,
    key: &[u8],
) -> Result<Box<[u8]>, GraphError> {
    transaction
        .get_with(Table::Adjacency, key, |value| {
            Ok::<_, GraphError>(Box::from(value))
        })?
        .ok_or(GraphError::Corrupt("an adjacency entry a scan met is gone"))
}

fn adjacency_edit<'a>(
    adjacency: &'a mut BTreeMap<SegmentId, SegmentEdit>,
    transaction: &impl ReadTransaction,
    id: SegmentId,
) -> Result<&'a mut SegmentEdit, GraphError> {
    match adjacency.entry(id) {
        btree_map::Entry::Occupied(held) => Ok(held.into_mut()),
        btree_map::Entry::Vacant(vacant) => {
            let (start, end) = id.range();
            let mut entries = Vec::new();
            transaction.visit_range_peek(Table::Adjacency, &start, &end, |key, value| {
                entries.push((Box::from(key), Lazy::of(value)));
                Ok::<(), GraphError>(())
            })?;
            let edit = SegmentEdit::load(&id, entries, &|key| fetch_adjacency(transaction, key))?;
            Ok(vacant.insert(edit))
        }
    }
}

fn typed(rel_type: u32, emit: &mut impl FnMut(Adjacent)) -> impl FnMut(Edge) + '_ {
    move |edge| {
        emit(Adjacent {
            relationship: edge.relationship,
            rel_type: Token::new(rel_type),
            other: edge.other,
        });
    }
}

/// How a visit of node meta ends early: its visitor stopped it, or it failed.
pub(crate) enum Flow<E> {
    Stop,
    Failed(E),
}

impl<E: From<GraphError>> From<GraphError> for Flow<E> {
    fn from(error: GraphError) -> Self {
        Self::Failed(error.into())
    }
}

impl<E: From<GraphError>> From<StorageError> for Flow<E> {
    fn from(error: StorageError) -> Self {
        Self::Failed(GraphError::from(error).into())
    }
}

fn live_entry(entry: Meta<'_>, ids: &mut Vec<NodeId>) -> Result<(), GraphError> {
    match entry {
        Meta::Chunk { group, bytes } => ids.extend(
            MetaBytes::parse(group, bytes)?
                .live_codes()
                .map(|(slot, _)| meta::node_at(group, slot)),
        ),
        Meta::Delta { group, delta } => amend(
            ids,
            meta::node_at(group, delta.slot),
            delta.before.is_some(),
            delta.after.is_some(),
        )?,
        Meta::Edited { group, edit } => {
            ids.extend(edit.live().map(|(slot, _)| meta::node_at(group, slot)));
        }
    }
    Ok(())
}

fn amend(ids: &mut Vec<NodeId>, node: NodeId, was: bool, is: bool) -> Result<(), GraphError> {
    match (ids.binary_search(&node), was, is) {
        (Ok(at), true, false) => {
            ids.remove(at);
        }
        (Err(at), false, true) => ids.insert(at, node),
        (Ok(_), true, true) | (Err(_), false, false) => {}
        _ => {
            return Err(GraphError::Corrupt(
                "a node meta delta does not amend its chunk",
            ));
        }
    }
    Ok(())
}
