use std::error::Error;
use std::fmt;

use crate::storage::ReadTransaction;
use crate::storage::Storage;
use crate::storage::StorageError;
use crate::storage::Table;
use crate::storage::WriteTransaction;
use crate::value::DecodeError;
use crate::value::PropertyValue;
use crate::value::Reader;

mod ids;
pub(crate) mod integrity;
mod keys;
mod records;

pub(crate) use ids::Label;
pub(crate) use ids::NodeId;
pub(crate) use ids::PropertyKey;
pub(crate) use ids::RelationshipId;
pub(crate) use ids::RelationshipType;
pub(crate) use ids::Token;
pub(crate) use ids::TokenKind;
pub(crate) use records::NodeRecord;
pub(crate) use records::RelationshipRecord;

const FORMAT_VERSION: u32 = 1;

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
}

impl<S: Storage> Graph<S> {
    /// Records the format version on first open and rejects a database written in another version.
    pub(crate) fn open(storage: S) -> Result<Self, GraphError> {
        {
            let mut transaction = storage.begin_write()?;
            match transaction.get(Table::Metadata, keys::FORMAT_VERSION_KEY)? {
                None => {
                    transaction.put(
                        Table::Metadata,
                        keys::FORMAT_VERSION_KEY,
                        &FORMAT_VERSION.to_be_bytes(),
                    )?;
                    transaction.commit()?;
                }
                Some(bytes) => {
                    let found = decode_u32(&bytes)?;
                    if found != FORMAT_VERSION {
                        return Err(GraphError::UnsupportedFormatVersion(found));
                    }
                }
            }
        }
        Ok(Self { storage })
    }

    pub(crate) fn read(&self) -> Result<GraphTx<S::Read<'_>>, GraphError> {
        Ok(GraphTx {
            transaction: self.storage.begin_read()?,
        })
    }

    pub(crate) fn write(&self) -> Result<GraphTx<S::Write<'_>>, GraphError> {
        Ok(GraphTx {
            transaction: self.storage.begin_write()?,
        })
    }
}

/// The graph operations over one storage transaction. Reads need a read transaction, writes a write transaction.
#[derive(Debug)]
pub(crate) struct GraphTx<T> {
    transaction: T,
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
        let bytes = self
            .transaction
            .get(Table::Nodes, &keys::element_key(id.0))?;
        Ok(bytes.map(|bytes| NodeRecord::decode(&bytes)).transpose()?)
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
        self.transaction
            .scan_prefix(table, &keys::adjacency_prefix(node, rel_type))?
            .iter()
            .map(|(key, value)| {
                let (rel_type, relationship) = keys::parse_adjacency_key(key)?;
                Ok(Adjacent {
                    relationship,
                    rel_type,
                    other: NodeId(decode_u64(value)?),
                })
            })
            .collect()
    }

    /// Looks a name up without creating it, so a read never writes.
    pub(crate) fn lookup_token<K: TokenKind>(
        &self,
        name: &str,
    ) -> Result<Option<Token<K>>, GraphError> {
        let bytes = self
            .transaction
            .get(Table::TokenByName, &keys::token_name_key(K::TAG, name))?;
        Ok(bytes
            .map(|bytes| decode_u32(&bytes).map(Token::new))
            .transpose()?)
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
        Ok(self.transaction.commit()?)
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
        Ok(())
    }

    /// Fails with `NodeHasRelationships` while any relationship still touches the node.
    pub(crate) fn delete_node(&mut self, id: NodeId) -> Result<(), GraphError> {
        let record = self.node(id)?.ok_or(GraphError::NodeNotFound(id))?;
        if !self.adjacent(id, Direction::Both, None)?.is_empty() {
            return Err(GraphError::NodeHasRelationships(id));
        }
        for label in record.labels {
            self.transaction
                .delete(Table::LabelIndex, &keys::label_key(label, id))?;
        }
        self.transaction
            .delete(Table::Nodes, &keys::element_key(id.0))?;
        Ok(())
    }

    fn require_node(&self, id: NodeId) -> Result<(), GraphError> {
        self.node(id)?
            .map(|_| ())
            .ok_or(GraphError::NodeNotFound(id))
    }

    fn edit_node<R>(
        &mut self,
        id: NodeId,
        edit: impl FnOnce(&mut NodeRecord) -> R,
    ) -> Result<R, GraphError> {
        let mut record = self.node(id)?.ok_or(GraphError::NodeNotFound(id))?;
        let result = edit(&mut record);
        self.transaction
            .put(Table::Nodes, &keys::element_key(id.0), &record.encode())?;
        Ok(result)
    }

    fn edit_relationship<R>(
        &mut self,
        id: RelationshipId,
        edit: impl FnOnce(&mut RelationshipRecord) -> R,
    ) -> Result<R, GraphError> {
        let mut record = self
            .relationship(id)?
            .ok_or(GraphError::RelationshipNotFound(id))?;
        let result = edit(&mut record);
        self.transaction.put(
            Table::Relationships,
            &keys::element_key(id.0),
            &record.encode(),
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
