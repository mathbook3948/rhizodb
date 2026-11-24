use crate::storage::ReadTransaction;
use crate::storage::Table;
use crate::storage::WriteTransaction;

use super::ColumnReader;
use super::GraphError;
use super::GraphTx;
use super::Label;
use super::NodeId;
use super::NodeRecord;
use super::Properties;
use super::RelationshipId;
use super::RelationshipRecord;
use super::Token;
use super::amend;
use super::decode_u64;
use super::keys;
use super::meta;
use super::meta::Meta;
use super::meta::MetaBytes;

impl<T: ReadTransaction> GraphTx<T> {
    /// The IDs the next created node and relationship take.
    pub(crate) fn next_ids(&self) -> Result<(u64, u64), GraphError> {
        Ok((
            self.stored_counter(keys::NEXT_NODE_ID_KEY)?,
            self.stored_counter(keys::NEXT_RELATIONSHIP_ID_KEY)?,
        ))
    }

    fn stored_counter(&self, key: &[u8]) -> Result<u64, GraphError> {
        match self.transaction.get(Table::Metadata, key)? {
            Some(bytes) => Ok(decode_u64(&bytes)?),
            None => Ok(0),
        }
    }

    /// Calls `visit` with every node in ascending ID order, with its labels and properties,
    /// holding the IDs of one row group at a time.
    pub(crate) fn visit_nodes(
        &self,
        mut visit: impl FnMut(NodeId, &[Token<Label>], Properties) -> Result<(), GraphError>,
    ) -> Result<(), GraphError> {
        let mut group = None;
        let mut ids = Vec::new();
        let mut flush = |ids: &mut Vec<NodeId>| -> Result<(), GraphError> {
            let mut reader = ColumnReader::default();
            for id in ids.drain(..) {
                let labels = self
                    .node_labels(id)?
                    .ok_or(GraphError::Corrupt("a live node has no labels"))?;
                let properties = self.live_node_properties(&mut reader, id)?;
                visit(id, &labels, properties)?;
            }
            Ok(())
        };
        self.visit_meta(true, |entry| {
            let at = match &entry {
                Meta::Chunk { group, .. }
                | Meta::Delta { group, .. }
                | Meta::Edited { group, .. } => *group,
            };
            if group != Some(at) {
                flush(&mut ids)?;
                group = Some(at);
            }
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
        flush(&mut ids)
    }

    /// Calls `visit` with every relationship in ascending ID order.
    pub(crate) fn visit_relationships(
        &self,
        mut visit: impl FnMut(RelationshipId, RelationshipRecord) -> Result<(), GraphError>,
    ) -> Result<(), GraphError> {
        self.visit_relationship_records(|id, record| visit(id, RelationshipRecord::decode(record)?))
    }
}

impl<T: WriteTransaction> GraphTx<T> {
    fn advance_counter(&mut self, key: &[u8], next: u64) -> Result<(), GraphError> {
        let current = self.next_counter(key)?;
        let held = self
            .counters
            .iter_mut()
            .find(|(held, _)| held == key)
            .ok_or(GraphError::Corrupt("a counter vanished"))?;
        if next < current {
            held.1 = current;
            return Err(GraphError::Corrupt("an ID below the next one was claimed"));
        }
        held.1 = next;
        Ok(())
    }

    /// Creates a node under the ID `id`, which must not be below the next node ID.
    pub(crate) fn create_node_at(
        &mut self,
        id: NodeId,
        record: NodeRecord,
    ) -> Result<(), GraphError> {
        self.advance_counter(keys::NEXT_NODE_ID_KEY, id.0)?;
        let created = self.create_node(record)?;
        if created == id {
            Ok(())
        } else {
            Err(GraphError::Corrupt("a node was created under another ID"))
        }
    }

    /// Creates a relationship under the ID `id`, which must not be below the next relationship
    /// ID.
    pub(crate) fn create_relationship_at(
        &mut self,
        id: RelationshipId,
        record: &RelationshipRecord,
    ) -> Result<(), GraphError> {
        self.advance_counter(keys::NEXT_RELATIONSHIP_ID_KEY, id.0)?;
        let created = self.create_relationship(record)?;
        if created == id {
            Ok(())
        } else {
            Err(GraphError::Corrupt(
                "a relationship was created under another ID",
            ))
        }
    }

    /// Moves the node and relationship counters up to `nodes` and `relationships`.
    pub(crate) fn advance_ids(&mut self, nodes: u64, relationships: u64) -> Result<(), GraphError> {
        self.advance_counter(keys::NEXT_NODE_ID_KEY, nodes)?;
        self.advance_counter(keys::NEXT_RELATIONSHIP_ID_KEY, relationships)
    }
}
