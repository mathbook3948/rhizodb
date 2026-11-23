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

mod ids;
mod index;
#[cfg(test)]
pub(crate) mod integrity;
mod keys;
mod records;
mod tokenize;
mod value_key;

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
pub(crate) use records::EncodedNode;
pub(crate) use records::NodeRecord;
pub(crate) use records::Properties;
pub(crate) use records::RelationshipRecord;
pub(crate) use value_key::Bound;
pub(crate) use value_key::Compared;
pub(crate) use value_key::KeyRange;
pub(crate) use value_key::compared;

use index::Element;

const FORMAT_VERSION: u32 = 6;
const OLDEST_FORMAT_VERSION: u32 = 1;
const POSITIONAL_POSTING_FORMAT_VERSION: u32 = 4;
const TAGGED_INDEX_TARGET_FORMAT_VERSION: u32 = 5;

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
}

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
            let mut tx = GraphTx {
                transaction: storage.begin_write()?,
                _writer: None,
                index_definitions: OnceLock::new(),
                catalog: None,
                tokens: Arc::default(),
                reads_committed_tokens: false,
            };
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
        let definitions = GraphTx {
            transaction: storage.begin_read()?,
            _writer: None,
            index_definitions: OnceLock::new(),
            catalog: None,
            tokens: Arc::default(),
            reads_committed_tokens: false,
        }
        .index_definitions()?;
        Ok(Self {
            storage,
            writer: Arc::default(),
            committed_index_definitions: Arc::new(Mutex::new(definitions)),
            committed_tokens: Arc::default(),
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
            _writer: Some(WriterClaim(Arc::clone(&self.writer))),
            index_definitions: OnceLock::from(definitions),
            catalog: Some(Catalog {
                shared: Arc::clone(&self.committed_index_definitions),
                changed: false,
            }),
            tokens: Arc::clone(&self.committed_tokens),
            reads_committed_tokens: false,
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
}

#[derive(Debug)]
struct Catalog {
    shared: Arc<Mutex<Arc<[Vec<u8>]>>>,
    changed: bool,
}

#[derive(Debug)]
struct WriterClaim(Arc<Mutex<Option<ThreadId>>>);

impl Drop for WriterClaim {
    fn drop(&mut self) {
        *lock(&self.0) = None;
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
    pub(crate) fn node(&self, id: NodeId) -> Result<Option<NodeRecord>, GraphError> {
        self.with_node(id, |node| node.decode())
    }

    /// A node's stored record, left encoded so the caller decodes only the parts it reads.
    /// Calls `read` with the stored record of the node with ID `id`, if there is one.
    pub(crate) fn with_node<R, E: From<GraphError>>(
        &self,
        id: NodeId,
        read: impl FnOnce(EncodedNode<'_>) -> Result<R, E>,
    ) -> Result<Option<R>, E> {
        self.transaction
            .get_with(Table::Nodes, &keys::element_key(id.0), |bytes| {
                Ok::<_, GraphError>(read(EncodedNode(bytes)))
            })
            .map_err(E::from)?
            .transpose()
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

    pub(crate) fn node_ids(&self) -> Result<Vec<NodeId>, GraphError> {
        self.transaction
            .scan_prefix(Table::Nodes, &[])?
            .iter()
            .map(|(key, _)| Ok(NodeId(decode_u64(key)?)))
            .collect()
    }

    pub(crate) fn node_ids_with_label(
        &self,
        label: Token<Label>,
    ) -> Result<Vec<NodeId>, GraphError> {
        self.transaction
            .scan_prefix(Table::LabelIndex, &keys::label_prefix(label))?
            .iter()
            .map(|(key, _)| Ok(keys::parse_label_key(key)?))
            .collect()
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
        if let Some(token) = self.tokens.get(name) {
            return Ok(Some(token));
        }
        let bytes = self
            .transaction
            .get(Table::TokenByName, &keys::token_name_key(K::TAG, name))?;
        let token = bytes
            .map(|bytes| decode_u32(&bytes).map(Token::new))
            .transpose()?;
        if let Some(token) = token
            && self.reads_committed_tokens
        {
            self.tokens.insert(name, token);
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
    pub(crate) fn commit(self) -> Result<(), GraphError> {
        let Some(Catalog {
            shared,
            changed: true,
        }) = &self.catalog
        else {
            return Ok(self.transaction.commit()?);
        };
        let definitions = self.index_definitions()?;
        let mut current = lock(shared);
        self.transaction.commit()?;
        *current = definitions;
        Ok(())
    }

    pub(crate) fn rollback(self) -> Result<(), GraphError> {
        Ok(self.transaction.rollback()?)
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
        Ok(Token::new(id))
    }

    pub(crate) fn create_node(&mut self, record: &NodeRecord) -> Result<NodeId, GraphError> {
        let id = NodeId(self.next_counter(keys::NEXT_NODE_ID_KEY)?);
        self.transaction
            .put(Table::Nodes, &keys::element_key(id.0), &record.encode())?;
        for label in &record.labels {
            self.transaction
                .put(Table::LabelIndex, &keys::label_key(*label, id), &[])?;
        }
        self.reindex(id.0, None, Some(Element::Node(record)))?;
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
        self.edit_node(node, |record| {
            record.properties.insert(key, value);
        })
    }

    pub(crate) fn remove_node_property(
        &mut self,
        node: NodeId,
        key: Token<PropertyKey>,
    ) -> Result<(), GraphError> {
        self.edit_node(node, |record| {
            record.properties.remove(&key);
        })
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
        let added = self.edit_node(node, |record| record.labels.insert(label))?;
        if added {
            self.transaction
                .put(Table::LabelIndex, &keys::label_key(label, node), &[])?;
        }
        Ok(())
    }

    pub(crate) fn remove_label(
        &mut self,
        node: NodeId,
        label: Token<Label>,
    ) -> Result<(), GraphError> {
        let removed = self.edit_node(node, |record| record.labels.remove(&label))?;
        if removed {
            self.transaction
                .delete(Table::LabelIndex, &keys::label_key(label, node))?;
        }
        Ok(())
    }

    pub(crate) fn delete_relationship(&mut self, id: RelationshipId) -> Result<(), GraphError> {
        let record = self
            .relationship(id)?
            .ok_or(GraphError::RelationshipNotFound(id))?;
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
        self.reindex(id.0, Some(Element::Relationship(&record)), None)
    }

    /// Removes the node with its label and property index entries. Relationships that touch it
    /// are left alone, so the caller must delete them or reject the transaction, which
    /// `ensure_detached` checks.
    pub(crate) fn delete_node(&mut self, id: NodeId) -> Result<(), GraphError> {
        let record = self.node(id)?.ok_or(GraphError::NodeNotFound(id))?;
        self.reindex(id.0, Some(Element::Node(&record)), None)?;
        for label in record.labels {
            self.transaction
                .delete(Table::LabelIndex, &keys::label_key(label, id))?;
        }
        self.transaction
            .delete(Table::Nodes, &keys::element_key(id.0))?;
        Ok(())
    }

    fn require_node(&self, id: NodeId) -> Result<(), GraphError> {
        self.with_node(id, |_| Ok::<_, GraphError>(()))?
            .ok_or(GraphError::NodeNotFound(id))
    }

    fn edit_node<R>(
        &mut self,
        id: NodeId,
        edit: impl FnOnce(&mut NodeRecord) -> R,
    ) -> Result<R, GraphError> {
        let before = self.node(id)?.ok_or(GraphError::NodeNotFound(id))?;
        let mut record = before.clone();
        let result = edit(&mut record);
        self.transaction
            .put(Table::Nodes, &keys::element_key(id.0), &record.encode())?;
        self.reindex(
            id.0,
            Some(Element::Node(&before)),
            Some(Element::Node(&record)),
        )?;
        Ok(result)
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
        let current = match self.transaction.get(Table::Metadata, key)? {
            Some(bytes) => decode_u64(&bytes)?,
            None => 0,
        };
        let next = current.checked_add(1).ok_or(GraphError::IdSpaceExhausted)?;
        self.transaction
            .put(Table::Metadata, key, &next.to_be_bytes())?;
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
