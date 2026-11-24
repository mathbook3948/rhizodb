use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::PoisonError;
use std::sync::RwLock;
use std::thread;
use std::thread::ThreadId;

use crate::storage::ReadTransaction;
use crate::storage::Storage;
use crate::storage::StorageError;
use crate::storage::Table;
use crate::storage::WriteTransaction;
use crate::value::DecodeError;
use crate::value::PropertyValue;
use crate::value::Reader;

mod columns;
mod ids;
mod index;
#[cfg(test)]
pub(crate) mod integrity;
mod keys;
mod meta;
mod properties;
mod records;
mod tokenize;
mod value_key;

pub(crate) use columns::ColumnReader;
pub(crate) use ids::Label;
pub use ids::NodeId;
pub(crate) use ids::PropertyKey;
pub use ids::RelationshipId;
pub(crate) use ids::RelationshipType;
pub(crate) use ids::Token;
pub(crate) use ids::TokenKind;
pub(crate) use index::ElementKind;
pub use index::IndexTarget;
pub(crate) use index::Target;
pub use index::VectorMetric;
pub(crate) use meta::LabelReader;
pub(crate) use records::NodeRecord;
pub(crate) use records::Properties;
pub(crate) use records::RelationshipRecord;
pub(crate) use value_key::Bound;
pub(crate) use value_key::Compared;
pub(crate) use value_key::KeyRange;
pub(crate) use value_key::compared;

use columns::ColumnsEdit;
use index::Element;
use meta::Delta;
use meta::Flush;
use meta::GroupEdit;
use meta::LabelSetCache;
use meta::LabelSetId;
use meta::Meta;
use meta::MetaBytes;
use meta::MetaEdit;
use meta::MetaHead;
use meta::Part;
use meta::Slot;

const FORMAT_VERSION: u32 = 9;
const OLDEST_FORMAT_VERSION: u32 = 1;
const POSITIONAL_POSTING_FORMAT_VERSION: u32 = 4;
const TAGGED_INDEX_TARGET_FORMAT_VERSION: u32 = 5;
const NODE_META_FORMAT_VERSION: u32 = 8;
const NODE_COLUMNS_FORMAT_VERSION: u32 = 9;

#[derive(Debug)]
pub(crate) enum GraphError {
    Storage(StorageError),
    Decode(DecodeError),
    Corrupt(&'static str),
    UnsupportedFormatVersion(u32),
    IdSpaceExhausted,
    NodeNotFound(NodeId),
    RelationshipNotFound(RelationshipId),
    NodeHasRelationships(NodeId),
    WriterOnThisThread,
    IndexNotFound(String, &'static str),
    IndexExists(String),
    InvalidIndex(&'static str),
    InvalidQueryVector(String),
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

#[derive(Debug)]
pub(crate) struct Graph<S> {
    storage: S,
    writer: Arc<Mutex<Option<ThreadId>>>,
    committed_index_definitions: Arc<Mutex<Arc<[Vec<u8>]>>>,
    committed_tokens: Arc<CommittedTokens>,
    committed_label_sets: Arc<LabelSetCache>,
    committed_column_deltas: Arc<ColumnDeltas>,
}

type ColumnDeltas = Mutex<BTreeMap<u64, usize>>;

/// The tokens of every name space by name, as committed. A committed token never changes, and
/// no element of a snapshot taken before a token was committed refers to it.
#[derive(Debug, Default)]
pub(crate) struct CommittedTokens {
    by_name: RwLock<[HashMap<String, u32>; 3]>,
}

impl CommittedTokens {
    fn get<K: TokenKind>(&self, name: &str) -> Option<Token<K>> {
        let by_name = self.by_name.read().unwrap_or_else(PoisonError::into_inner);
        by_name
            .get(usize::from(K::TAG))?
            .get(name)
            .map(|id| Token::new(*id))
    }

    fn insert<K: TokenKind>(&self, name: &str, token: Token<K>) {
        let mut by_name = self.by_name.write().unwrap_or_else(PoisonError::into_inner);
        if let Some(names) = by_name.get_mut(usize::from(K::TAG)) {
            names.insert(name.to_owned(), token.id());
        }
    }
}

impl<S: Storage> Graph<S> {
    /// Records the format version on first open, upgrades a database written in an older
    /// version to the current one, and rejects a database written in a newer version. Index
    /// definitions written before indexes could cover relationships are rewritten, and keyword
    /// indexes written before postings kept positions or built by another tokenizer are rebuilt,
    /// in the same write transaction.
    pub(crate) fn open(storage: S) -> Result<Self, GraphError> {
        {
            let mut tx = GraphTx::bare(storage.begin_write()?);
            let found = match tx
                .transaction
                .get(Table::Metadata, keys::FORMAT_VERSION_KEY)?
            {
                None => None,
                Some(bytes) => Some(decode_u32(&bytes)?),
            };
            let upgrade = match found {
                Some(FORMAT_VERSION) => false,
                Some(found) if !(OLDEST_FORMAT_VERSION..FORMAT_VERSION).contains(&found) => {
                    return Err(GraphError::UnsupportedFormatVersion(found));
                }
                _ => true,
            };
            if found.is_some_and(|found| found < TAGGED_INDEX_TARGET_FORMAT_VERSION) {
                tx.tag_index_targets()?;
            }
            if found.is_some_and(|found| found < NODE_META_FORMAT_VERSION) {
                tx.move_labels_to_node_meta()?;
            }
            if found.is_some_and(|found| found < NODE_COLUMNS_FORMAT_VERSION) {
                tx.move_properties_to_columns()?;
            }
            if upgrade {
                tx.transaction.put(
                    Table::Metadata,
                    keys::FORMAT_VERSION_KEY,
                    &FORMAT_VERSION.to_be_bytes(),
                )?;
            }
            let stale_postings =
                found.is_some_and(|found| found < POSITIONAL_POSTING_FORMAT_VERSION);
            if tx.adopt_current_tokenizer(stale_postings)? || upgrade {
                tx.commit()?;
            }
        }
        let definitions = GraphTx::bare(storage.begin_read()?).index_definitions()?;
        Ok(Self {
            storage,
            writer: Arc::default(),
            committed_index_definitions: Arc::new(Mutex::new(definitions)),
            committed_tokens: Arc::default(),
            committed_label_sets: Arc::default(),
            committed_column_deltas: Arc::default(),
        })
    }

    pub(crate) fn read(&self) -> Result<GraphTx<S::Read<'_>>, GraphError> {
        let catalog = lock(&self.committed_index_definitions);
        let transaction = self.storage.begin_read()?;
        Ok(GraphTx {
            transaction,
            _writer: None,
            index_definitions: OnceLock::from(Arc::clone(&catalog)),
            catalog: None,
            tokens: Arc::clone(&self.committed_tokens),
            reads_committed_tokens: true,
            own_tokens: CommittedTokens::default(),
            label_sets: Arc::clone(&self.committed_label_sets),
            own_label_sets: LabelSetCache::default(),
            meta: BTreeMap::new(),
            columns: BTreeMap::new(),
            reader: ColumnReader::default(),
            column_deltas: None,
            own_column_deltas: BTreeMap::new(),
            counters: Vec::new(),
        })
    }

    /// Opens the one write transaction, waiting for another thread's to end. Fails instead of
    /// waiting forever when the calling thread already holds it.
    pub(crate) fn write(&self) -> Result<GraphTx<S::Write<'_>>, GraphError> {
        let current = thread::current().id();
        if *lock(&self.writer) == Some(current) {
            return Err(GraphError::WriterOnThisThread);
        }
        let transaction = self.storage.begin_write()?;
        *lock(&self.writer) = Some(current);
        let definitions = Arc::clone(&lock(&self.committed_index_definitions));
        Ok(GraphTx {
            transaction,
            _writer: Some(WriterClaim {
                slot: Arc::clone(&self.writer),
                owner: current,
            }),
            index_definitions: OnceLock::from(definitions),
            catalog: Some(Catalog {
                shared: Arc::clone(&self.committed_index_definitions),
                changed: false,
            }),
            tokens: Arc::clone(&self.committed_tokens),
            reads_committed_tokens: false,
            own_tokens: CommittedTokens::default(),
            label_sets: Arc::clone(&self.committed_label_sets),
            own_label_sets: LabelSetCache::default(),
            meta: BTreeMap::new(),
            columns: BTreeMap::new(),
            reader: ColumnReader::default(),
            column_deltas: Some(Arc::clone(&self.committed_column_deltas)),
            own_column_deltas: BTreeMap::new(),
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
    meta: BTreeMap<u64, GroupEdit>,
    columns: BTreeMap<u64, ColumnsEdit>,
    reader: ColumnReader,
    column_deltas: Option<Arc<ColumnDeltas>>,
    own_column_deltas: BTreeMap<u64, usize>,
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
            meta: BTreeMap::new(),
            columns: BTreeMap::new(),
            reader: ColumnReader::default(),
            column_deltas: None,
            own_column_deltas: BTreeMap::new(),
            counters: Vec::new(),
        }
    }
}

#[derive(Debug)]
struct Catalog {
    shared: Arc<Mutex<Arc<[Vec<u8>]>>>,
    changed: bool,
}

#[derive(Debug)]
struct WriterClaim {
    slot: Arc<Mutex<Option<ThreadId>>>,
    owner: ThreadId,
}

impl Drop for WriterClaim {
    fn drop(&mut self) {
        let mut slot = lock(&self.slot);
        if *slot == Some(self.owner) {
            *slot = None;
        }
    }
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
        let bytes = self
            .transaction
            .get(Table::Relationships, &keys::element_key(id.0))?;
        Ok(bytes
            .map(|bytes| RelationshipRecord::property(&bytes, key))
            .transpose()?)
    }

    pub(crate) fn relationship(
        &self,
        id: RelationshipId,
    ) -> Result<Option<RelationshipRecord>, GraphError> {
        let bytes = self
            .transaction
            .get(Table::Relationships, &keys::element_key(id.0))?;
        Ok(bytes
            .map(|bytes| RelationshipRecord::decode(&bytes))
            .transpose()?)
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
        mut visit: impl FnMut(Meta<'_>) -> Result<(), GraphError>,
    ) -> Result<(), GraphError> {
        let mut edits = self.meta.iter().peekable();
        let mut edited_group = None;
        let mut chunk_group = None;
        let mut chunk = Vec::new();
        self.transaction
            .visit_prefix(Table::NodeChunks, &[], |key, value| {
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
                            ));
                        }
                        visit(Meta::Delta { group, delta })
                    }
                    Part::Delta(_) => Err(GraphError::Corrupt("a node meta delta has no chunk")),
                }
            })?;
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
        reader.with_labels(group, slot, || self.load_meta(group), read)
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
        let mut ids = Vec::new();
        self.visit_meta(true, |entry| {
            match entry {
                Meta::Chunk { group, bytes } => ids.extend(
                    MetaBytes::parse(group, bytes)?
                        .live_codes()
                        .map(|(slot, _)| meta::node_at(group, slot)),
                ),
                Meta::Delta { group, delta } => amend(
                    &mut ids,
                    meta::node_at(group, delta.slot),
                    delta.before.is_some(),
                    delta.after.is_some(),
                )?,
                Meta::Edited { group, edit } => {
                    ids.extend(edit.live().map(|(slot, _)| meta::node_at(group, slot)));
                }
            }
            Ok(())
        })?;
        Ok(ids)
    }

    /// The nodes that carry `label`, in ascending ID order.
    pub(crate) fn node_ids_with_label(
        &self,
        label: Token<Label>,
    ) -> Result<Vec<NodeId>, GraphError> {
        let mut ids = Vec::new();
        self.visit_meta(true, |entry| {
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
                    &mut ids,
                    meta::node_at(group, delta.slot),
                    self.carries(delta.before, label)?,
                    self.carries(delta.after, label)?,
                )?,
                Meta::Edited { group, edit } => {
                    for (slot, set) in edit.live() {
                        if self.carries(Some(set), label)? {
                            ids.push(meta::node_at(group, slot));
                        }
                    }
                }
            }
            Ok(())
        })?;
        Ok(ids)
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
                    for (_, set) in edit.live() {
                        carrying += u64::from(self.carries(Some(set), label)?);
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

    /// The relationships touching `node`, optionally of one type. A self-loop is reported once under `Both`.
    pub(crate) fn adjacent(
        &self,
        node: NodeId,
        direction: Direction,
        rel_type: Option<Token<RelationshipType>>,
    ) -> Result<Vec<Adjacent>, GraphError> {
        let mut found = Vec::new();
        if direction != Direction::Incoming {
            found.extend(self.adjacency_entries(Table::Outgoing, node, rel_type)?);
        }
        if direction != Direction::Outgoing {
            let incoming = self.adjacency_entries(Table::Incoming, node, rel_type)?;
            let skip_self_loops = direction == Direction::Both;
            found.extend(
                incoming
                    .into_iter()
                    .filter(|adjacent| !(skip_self_loops && adjacent.other == node)),
            );
        }
        Ok(found)
    }

    fn adjacency_entries(
        &self,
        table: Table,
        node: NodeId,
        rel_type: Option<Token<RelationshipType>>,
    ) -> Result<Vec<Adjacent>, GraphError> {
        let mut found = Vec::new();
        self.transaction.visit_prefix(
            table,
            &keys::adjacency_prefix(node, rel_type),
            |key, value| {
                let (rel_type, relationship) = keys::parse_adjacency_key(key)?;
                found.push(Adjacent {
                    relationship,
                    rel_type,
                    other: NodeId(decode_u64(value)?),
                });
                Ok::<(), GraphError>(())
            },
        )?;
        Ok(found)
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
            if self.reads_committed_tokens {
                self.tokens.insert(name, token);
            } else {
                self.own_tokens.insert(name, token);
            }
        }
        Ok(token)
    }

    pub(crate) fn token_name<K: TokenKind>(&self, token: Token<K>) -> Result<String, GraphError> {
        let bytes = self
            .transaction
            .get(Table::TokenById, &keys::token_id_key(K::TAG, token.id()))?
            .ok_or(GraphError::Corrupt("token has no name"))?;
        String::from_utf8(bytes).map_err(|_| GraphError::Decode(DecodeError::InvalidUtf8))
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
        let mut current = definitions
            .as_ref()
            .map(|(shared, definitions)| (lock(shared), definitions));
        let mut counts = self.column_deltas.as_ref().map(|shared| lock(shared));
        let committed = self.transaction.commit();
        if let Some(counts) = &mut counts {
            if committed.is_ok() {
                counts.extend(std::mem::take(&mut self.own_column_deltas));
            } else {
                counts.clear();
            }
        }
        committed?;
        if let Some((current, definitions)) = &mut current {
            **current = Arc::clone(definitions);
        }
        Ok(())
    }

    pub(crate) fn rollback(self) -> Result<(), GraphError> {
        Ok(self.transaction.rollback()?)
    }

    fn flush(&mut self) -> Result<(), GraphError> {
        for (key, next) in std::mem::take(&mut self.counters) {
            self.transaction
                .put(Table::Metadata, &key, &next.to_be_bytes())?;
        }
        self.flush_columns()?;
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
                self.transaction
                    .put(Table::LabelSets, &id.to_be_bytes(), &key)?;
                self.transaction
                    .put(Table::LabelSetByTokens, &key, &id.to_be_bytes())?;
                LabelSetId(id)
            }
        };
        self.own_label_sets.insert(set, labels);
        Ok(set)
    }

    fn move_labels_to_node_meta(&mut self) -> Result<(), GraphError> {
        let mut groups = BTreeSet::new();
        self.transaction.visit_prefix(Table::Nodes, &[], |key, _| {
            groups.insert(meta::locate(NodeId(decode_u64(key)?)).0);
            Ok::<(), GraphError>(())
        })?;
        for group in groups {
            let start = keys::element_key(meta::node_at(group, 0).0);
            let end = group
                .checked_add(1)
                .and_then(|after| after.checked_mul(meta::SLOTS as u64))
                .map_or([0xff; 9].to_vec(), |after| {
                    keys::element_key(after).to_vec()
                });
            let mut edit = MetaEdit::default();
            for (key, value) in self.transaction.scan_range(Table::Nodes, &start, &end)? {
                let id = NodeId(decode_u64(&key)?);
                let record = NodeRecord::decode_with_labels(&value)?;
                for label in &record.labels {
                    self.transaction
                        .delete(Table::LabelIndex, &keys::label_key(*label, id))?;
                }
                self.transaction.put(Table::Nodes, &key, &record.encode())?;
                let set = self.intern_label_set(&record.labels)?;
                edit.put(meta::locate(id).1, set);
            }
            if let Some(chunk) = edit.encode(group, |set| self.label_set(set))? {
                self.transaction
                    .put(Table::NodeChunks, &meta::meta_key(group), &chunk)?;
            }
        }
        for (key, _) in self.transaction.scan_prefix(Table::LabelIndex, &[])? {
            self.transaction.delete(Table::LabelIndex, &key)?;
        }
        Ok(())
    }

    fn move_properties_to_columns(&mut self) -> Result<(), GraphError> {
        let mut group: Option<u64> = None;
        let mut values: BTreeMap<Token<PropertyKey>, Vec<(usize, PropertyValue)>> = BTreeMap::new();
        for (key, value) in self.transaction.scan_prefix(Table::Nodes, &[])? {
            let (at, slot) = meta::locate(NodeId(decode_u64(&key)?));
            if group.is_some_and(|group| group != at) {
                self.put_columns(group, std::mem::take(&mut values))?;
            }
            group = Some(at);
            for (token, value) in NodeRecord::decode_properties(&value)? {
                values.entry(token).or_default().push((slot, value));
            }
            self.transaction.delete(Table::Nodes, &key)?;
        }
        self.put_columns(group, values)
    }

    fn put_columns(
        &mut self,
        group: Option<u64>,
        values: BTreeMap<Token<PropertyKey>, Vec<(usize, PropertyValue)>>,
    ) -> Result<(), GraphError> {
        let Some(group) = group else {
            return Ok(());
        };
        for (token, values) in values {
            let mut start = 0;
            while start < values.len() {
                let segment = values[start].0 / columns::SEGMENT_SLOTS;
                let end = values[start..]
                    .iter()
                    .position(|(slot, _)| slot / columns::SEGMENT_SLOTS != segment)
                    .map_or(values.len(), |offset| start + offset);
                let cells: Vec<(usize, columns::Cell<'_>)> = values[start..end]
                    .iter()
                    .map(|(slot, value)| (slot % columns::SEGMENT_SLOTS, columns::Cell::of(value)))
                    .collect();
                for (key, bytes) in columns::encode_segment(group, token, segment, &cells)? {
                    self.transaction.put(Table::NodeColumns, &key, &bytes)?;
                }
                start = end;
            }
        }
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
        self.own_tokens.insert(name, token);
        Ok(token)
    }

    pub(crate) fn create_node(&mut self, record: NodeRecord) -> Result<NodeId, GraphError> {
        let id = NodeId(self.next_counter(keys::NEXT_NODE_ID_KEY)?);
        let set = self.intern_label_set(&record.labels)?;
        let (group, slot) = meta::locate(id);
        self.meta_edit(group)?.set(slot, Some(set))?;
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
        self.transaction.put(
            Table::Relationships,
            &keys::element_key(id.0),
            &record.encode(),
        )?;
        self.transaction.put(
            Table::Outgoing,
            &keys::adjacency_key(record.source, record.rel_type, id),
            &record.target.0.to_be_bytes(),
        )?;
        self.transaction.put(
            Table::Incoming,
            &keys::adjacency_key(record.target, record.rel_type, id),
            &record.source.0.to_be_bytes(),
        )?;
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

    pub(crate) fn add_label(
        &mut self,
        node: NodeId,
        label: Token<Label>,
    ) -> Result<(), GraphError> {
        self.relabel(node, label, true)
    }

    pub(crate) fn remove_label(
        &mut self,
        node: NodeId,
        label: Token<Label>,
    ) -> Result<(), GraphError> {
        self.relabel(node, label, false)
    }

    /// Removes the relationship and returns its record, or returns `None` when it does not exist.
    pub(crate) fn delete_relationship(
        &mut self,
        id: RelationshipId,
    ) -> Result<Option<RelationshipRecord>, GraphError> {
        let Some(record) = self.relationship(id)? else {
            return Ok(None);
        };
        self.transaction
            .delete(Table::Relationships, &keys::element_key(id.0))?;
        self.transaction.delete(
            Table::Outgoing,
            &keys::adjacency_key(record.source, record.rel_type, id),
        )?;
        self.transaction.delete(
            Table::Incoming,
            &keys::adjacency_key(record.target, record.rel_type, id),
        )?;
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
        self.meta_edit(group)?.set(slot, None)?;
        Ok(Some(record))
    }

    fn require_node(&self, id: NodeId) -> Result<(), GraphError> {
        self.node_label_set(id)?
            .map(|_| ())
            .ok_or(GraphError::NodeNotFound(id))
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

    fn write_node_property(
        &mut self,
        id: NodeId,
        key: Token<PropertyKey>,
        value: Option<PropertyValue>,
    ) -> Result<(), GraphError> {
        let labels = self.node_labels(id)?.ok_or(GraphError::NodeNotFound(id))?;
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

    fn relabel(&mut self, id: NodeId, label: Token<Label>, add: bool) -> Result<(), GraphError> {
        let labels = self.node_labels(id)?.ok_or(GraphError::NodeNotFound(id))?;
        if labels.binary_search(&label).is_ok() == add {
            return Ok(());
        }
        let indexed = self.node_index_keys(|covered, _| covered == label)?;
        let before = self.partial_record(id, &labels, &indexed)?;
        let mut after = before.clone();
        if add {
            after.labels.insert(label);
        } else {
            after.labels.remove(&label);
        }
        let set = self.intern_label_set(&after.labels)?;
        let (group, slot) = meta::locate(id);
        self.meta_edit(group)?.set(slot, Some(set))?;
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
        self.transaction.put(
            Table::Relationships,
            &keys::element_key(id.0),
            &record.encode(),
        )?;
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
#[path = "graph/upgrade.test.rs"]
mod upgrade_tests;

#[cfg(test)]
#[path = "graph/columns_model.test.rs"]
mod columns_model_tests;

#[cfg(test)]
#[path = "graph/columns_crash.test.rs"]
mod columns_crash_tests;

#[cfg(test)]
#[path = "graph/columns_corrupt.test.rs"]
mod columns_corrupt_tests;

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
