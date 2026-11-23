use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Arc;

use crate::storage::ReadTransaction;
use crate::storage::Table;

use super::FORMAT_VERSION;
use super::GraphError;
use super::GraphTx;
use super::NodeId;
use super::NodeRecord;
use super::RelationshipId;
use super::RelationshipRecord;
use super::decode_u32;
use super::decode_u64;
use super::ids::Label;
use super::ids::PropertyKey;
use super::ids::RelationshipType;
use super::ids::Token;
use super::ids::TokenKind;
use super::index::ENTRY_TABLES;
use super::index::Element;
use super::index::Entry;
use super::index::Index;
use super::index::IndexKind;
use super::index::Target;
use super::index::document_length;
use super::index::encode_totals;
use super::keys;
use super::meta;
use super::meta::LabelSetId;
use super::meta::MetaBytes;
use super::meta::MetaEdit;
use super::records::Properties;
use super::tokenize;

const TOKEN_TAGS: [u8; 3] = [Label::TAG, RelationshipType::TAG, PropertyKey::TAG];

/// A counter in the metadata table that must stay above every identifier it has handed out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Counter {
    NodeId,
    RelationshipId,
    Token(u8),
    IndexId,
    LabelSetId,
}

/// One way a stored graph breaks an invariant of the key layout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum IntegrityViolation {
    FormatVersionMissing,
    FormatVersionMismatch(u32),
    TokenizerMismatch,
    Undecodable {
        table: Table,
        key: Vec<u8>,
    },
    CounterNotAboveAllocated {
        counter: Counter,
        highest: u64,
    },
    UnknownTokenTag(u8),
    TokenTablesDisagree {
        tag: u8,
        id: u32,
    },
    UnnamedToken {
        tag: u8,
        id: u32,
    },
    DanglingEndpoint {
        relationship: RelationshipId,
        node: NodeId,
    },
    MissingAdjacency {
        table: Table,
        relationship: RelationshipId,
    },
    OrphanAdjacency {
        table: Table,
        node: NodeId,
        relationship: RelationshipId,
    },
    LabelSetTablesDisagree(u32),
    UnknownLabelSet {
        node: NodeId,
        set: u32,
    },
    MissingNodeMeta(NodeId),
    MissingNodeRecord(NodeId),
    NonCanonicalNodeMeta(u64),
    DuplicateIndexId(u32),
    MissingIndexEntry {
        table: Table,
        key: Vec<u8>,
    },
    UnexpectedIndexEntry {
        table: Table,
        key: Vec<u8>,
    },
    WrongIndexEntry {
        table: Table,
        key: Vec<u8>,
    },
}

type LabelSets = BTreeMap<LabelSetId, Arc<[Token<Label>]>>;

type Adjacency = BTreeMap<(NodeId, u32, RelationshipId), NodeId>;

impl<T: ReadTransaction> GraphTx<T> {
    /// Checks every invariant the key layout implies and returns all violations found. An empty list means the graph is consistent.
    pub(crate) fn check_integrity(&self) -> Result<Vec<IntegrityViolation>, GraphError> {
        let mut violations = Vec::new();
        self.check_format_version(&mut violations)?;
        let named = self.check_tokens(&mut violations)?;
        let label_sets = self.load_label_sets(&mut violations)?;
        let nodes = self.load_nodes(&label_sets, &mut violations)?;
        let relationships = self.load_relationships(&mut violations)?;
        let outgoing = self.load_adjacency(Table::Outgoing, &mut violations)?;
        let incoming = self.load_adjacency(Table::Incoming, &mut violations)?;
        let indexes = self.load_indexes(&mut violations)?;

        let mut check_named = |tag: u8, id: u32| {
            if !named.contains(&(tag, id)) {
                violations.push(IntegrityViolation::UnnamedToken { tag, id });
            }
        };
        let property_sets = nodes
            .values()
            .map(|record| &record.properties)
            .chain(relationships.values().map(|record| &record.properties));
        for properties in property_sets {
            check_properties(properties, &mut check_named);
        }
        for labels in label_sets.values() {
            for label in labels.iter() {
                check_named(Label::TAG, label.id());
            }
        }
        for record in relationships.values() {
            check_named(RelationshipType::TAG, record.rel_type.id());
        }
        for index in &indexes {
            match index.definition.target {
                Target::Label(label) => check_named(Label::TAG, label.id()),
                Target::RelationshipType(rel_type) => {
                    check_named(RelationshipType::TAG, rel_type.id());
                }
            }
            for property in covered_properties(&index.definition.kind) {
                check_named(PropertyKey::TAG, property.id());
            }
        }

        for (id, record) in &relationships {
            for node in [record.source, record.target] {
                if !nodes.contains_key(&node) {
                    violations.push(IntegrityViolation::DanglingEndpoint {
                        relationship: *id,
                        node,
                    });
                }
            }
            let expected = [
                (Table::Outgoing, &outgoing, record.source, record.target),
                (Table::Incoming, &incoming, record.target, record.source),
            ];
            for (table, entries, owner, other) in expected {
                if entries.get(&(owner, record.rel_type.id(), *id)) != Some(&other) {
                    violations.push(IntegrityViolation::MissingAdjacency {
                        table,
                        relationship: *id,
                    });
                }
            }
        }
        for (table, entries) in [(Table::Outgoing, &outgoing), (Table::Incoming, &incoming)] {
            for ((node, rel_type, relationship), other) in entries {
                let matches = relationships.get(relationship).is_some_and(|record| {
                    let (owner, far) = if table == Table::Outgoing {
                        (record.source, record.target)
                    } else {
                        (record.target, record.source)
                    };
                    owner == *node && far == *other && record.rel_type.id() == *rel_type
                });
                if !matches {
                    violations.push(IntegrityViolation::OrphanAdjacency {
                        table,
                        node: *node,
                        relationship: *relationship,
                    });
                }
            }
        }

        self.check_index_entries(&indexes, &nodes, &relationships, &mut violations)?;

        self.check_counters(
            &nodes,
            &relationships,
            &named,
            &indexes,
            &label_sets,
            &mut violations,
        )?;
        Ok(violations)
    }

    fn check_format_version(
        &self,
        violations: &mut Vec<IntegrityViolation>,
    ) -> Result<(), GraphError> {
        match self
            .transaction
            .get(Table::Metadata, keys::FORMAT_VERSION_KEY)?
        {
            None => violations.push(IntegrityViolation::FormatVersionMissing),
            Some(bytes) => match decode_u32(&bytes) {
                Ok(FORMAT_VERSION) => {}
                Ok(found) => violations.push(IntegrityViolation::FormatVersionMismatch(found)),
                Err(_) => violations.push(IntegrityViolation::Undecodable {
                    table: Table::Metadata,
                    key: keys::FORMAT_VERSION_KEY.to_vec(),
                }),
            },
        }
        let tokenizer = self.transaction.get(Table::Metadata, keys::TOKENIZER_KEY)?;
        if tokenizer != Some(tokenize::fingerprint()) {
            violations.push(IntegrityViolation::TokenizerMismatch);
        }
        Ok(())
    }

    fn check_tokens(
        &self,
        violations: &mut Vec<IntegrityViolation>,
    ) -> Result<BTreeSet<(u8, u32)>, GraphError> {
        let mut by_name = BTreeMap::new();
        for (key, value) in self.transaction.scan_prefix(Table::TokenByName, &[])? {
            let parsed = key
                .split_first()
                .and_then(|(tag, name)| Some((*tag, String::from_utf8(name.to_vec()).ok()?)))
                .zip(decode_u32(&value).ok());
            match parsed {
                Some((token, id)) => {
                    by_name.insert(token, id);
                }
                None => violations.push(IntegrityViolation::Undecodable {
                    table: Table::TokenByName,
                    key,
                }),
            }
        }
        let mut by_id = BTreeMap::new();
        for (key, value) in self.transaction.scan_prefix(Table::TokenById, &[])? {
            let parsed = <[u8; 5]>::try_from(key.as_slice())
                .ok()
                .map(|bytes| {
                    let id = u32::from_be_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]);
                    (bytes[0], id)
                })
                .zip(String::from_utf8(value).ok());
            match parsed {
                Some((token, name)) => {
                    by_id.insert(token, name);
                }
                None => violations.push(IntegrityViolation::Undecodable {
                    table: Table::TokenById,
                    key,
                }),
            }
        }
        let tags: BTreeSet<u8> = by_name
            .keys()
            .map(|(tag, _)| *tag)
            .chain(by_id.keys().map(|(tag, _)| *tag))
            .collect();
        for tag in tags.iter().filter(|tag| !TOKEN_TAGS.contains(tag)) {
            violations.push(IntegrityViolation::UnknownTokenTag(*tag));
        }
        for ((tag, name), id) in &by_name {
            if by_id.get(&(*tag, *id)) != Some(name) {
                violations.push(IntegrityViolation::TokenTablesDisagree { tag: *tag, id: *id });
            }
        }
        for ((tag, id), name) in &by_id {
            if by_name.get(&(*tag, name.clone())) != Some(id) {
                violations.push(IntegrityViolation::TokenTablesDisagree { tag: *tag, id: *id });
            }
        }
        Ok(by_id.into_keys().collect())
    }

    fn load_label_sets(
        &self,
        violations: &mut Vec<IntegrityViolation>,
    ) -> Result<LabelSets, GraphError> {
        let mut by_id = BTreeMap::new();
        for (key, value) in self.transaction.scan_prefix(Table::LabelSets, &[])? {
            let parsed = <[u8; 4]>::try_from(key.as_slice())
                .ok()
                .map(u32::from_be_bytes)
                .filter(|id| *id != LabelSetId::EMPTY.0)
                .zip(meta::parse_label_set(&value).ok());
            match parsed {
                Some((id, labels)) => {
                    by_id.insert(LabelSetId(id), labels);
                }
                None => violations.push(IntegrityViolation::Undecodable {
                    table: Table::LabelSets,
                    key,
                }),
            }
        }
        let mut by_key = BTreeMap::new();
        for (key, value) in self.transaction.scan_prefix(Table::LabelSetByTokens, &[])? {
            match (meta::parse_label_set(&key), decode_u32(&value)) {
                (Ok(_), Ok(id)) => {
                    by_key.insert(key, id);
                }
                _ => violations.push(IntegrityViolation::Undecodable {
                    table: Table::LabelSetByTokens,
                    key,
                }),
            }
        }
        for (id, labels) in &by_id {
            if by_key.get(&meta::label_set_key(labels)) != Some(&id.0) {
                violations.push(IntegrityViolation::LabelSetTablesDisagree(id.0));
            }
        }
        for (key, id) in &by_key {
            let agrees = by_id
                .get(&LabelSetId(*id))
                .is_some_and(|labels| meta::label_set_key(labels) == *key);
            if !agrees {
                violations.push(IntegrityViolation::LabelSetTablesDisagree(*id));
            }
        }
        by_id.insert(LabelSetId::EMPTY, Arc::from([]));
        Ok(by_id)
    }

    fn load_nodes(
        &self,
        label_sets: &LabelSets,
        violations: &mut Vec<IntegrityViolation>,
    ) -> Result<BTreeMap<NodeId, NodeRecord>, GraphError> {
        let mut chunks: BTreeMap<Vec<u8>, Vec<u8>> = self
            .transaction
            .scan_prefix(Table::NodeChunks, &[])?
            .into_iter()
            .collect();
        for (group, edit) in &self.meta {
            let key = meta::meta_key(*group).to_vec();
            let pending = edit.encode(|set| self.label_set(set))?;
            match pending {
                Some(value) => chunks.insert(key, value),
                None => chunks.remove(&key),
            };
        }
        let mut live = BTreeMap::new();
        for (key, value) in chunks {
            let decoded = meta::parse_meta_key(&key).ok().flatten().zip(
                MetaBytes::parse(&value)
                    .ok()
                    .and_then(|bytes| bytes.to_edit().ok()),
            );
            let Some((group, edit)) = decoded else {
                violations.push(IntegrityViolation::Undecodable {
                    table: Table::NodeChunks,
                    key,
                });
                continue;
            };
            let mut known = true;
            for (slot, set) in edit.live() {
                let node = meta::node_at(group, slot);
                if !label_sets.contains_key(&set) {
                    known = false;
                    violations.push(IntegrityViolation::UnknownLabelSet { node, set: set.0 });
                }
                live.insert(node, set);
            }
            if known && canonical(&edit, label_sets)? != Some(value) {
                violations.push(IntegrityViolation::NonCanonicalNodeMeta(group));
            }
        }
        let mut nodes = BTreeMap::new();
        for (key, value) in self.transaction.scan_prefix(Table::Nodes, &[])? {
            let (Ok(id), Ok(properties)) =
                (decode_u64(&key), NodeRecord::decode_properties(&value))
            else {
                violations.push(IntegrityViolation::Undecodable {
                    table: Table::Nodes,
                    key,
                });
                continue;
            };
            let id = NodeId(id);
            let Some(set) = live.remove(&id) else {
                violations.push(IntegrityViolation::MissingNodeMeta(id));
                continue;
            };
            let labels = label_sets
                .get(&set)
                .map(|labels| labels.iter().copied().collect())
                .unwrap_or_default();
            nodes.insert(id, NodeRecord { labels, properties });
        }
        for node in live.into_keys() {
            violations.push(IntegrityViolation::MissingNodeRecord(node));
        }
        Ok(nodes)
    }

    fn load_relationships(
        &self,
        violations: &mut Vec<IntegrityViolation>,
    ) -> Result<BTreeMap<RelationshipId, RelationshipRecord>, GraphError> {
        let mut relationships = BTreeMap::new();
        for (key, value) in self.transaction.scan_prefix(Table::Relationships, &[])? {
            match (decode_u64(&key), RelationshipRecord::decode(&value)) {
                (Ok(id), Ok(record)) => {
                    relationships.insert(RelationshipId(id), record);
                }
                _ => violations.push(IntegrityViolation::Undecodable {
                    table: Table::Relationships,
                    key,
                }),
            }
        }
        Ok(relationships)
    }

    fn load_adjacency(
        &self,
        table: Table,
        violations: &mut Vec<IntegrityViolation>,
    ) -> Result<Adjacency, GraphError> {
        let mut entries = Adjacency::new();
        for (key, value) in self.transaction.scan_prefix(table, &[])? {
            match (keys::parse_adjacency_key(&key), decode_u64(&value)) {
                (Ok((rel_type, relationship)), Ok(other)) => {
                    let node = NodeId(decode_u64(&key[..8])?);
                    entries.insert((node, rel_type.id(), relationship), NodeId(other));
                }
                _ => violations.push(IntegrityViolation::Undecodable { table, key }),
            }
        }
        Ok(entries)
    }

    fn load_indexes(
        &self,
        violations: &mut Vec<IntegrityViolation>,
    ) -> Result<Vec<Index>, GraphError> {
        let mut indexes: Vec<Index> = Vec::new();
        for (key, value) in self.transaction.scan_prefix(Table::Indexes, &[])? {
            match (String::from_utf8(key.clone()), Index::decode(&value)) {
                (Ok(_), Ok(index)) => {
                    if indexes.iter().any(|held| held.id == index.id) {
                        violations.push(IntegrityViolation::DuplicateIndexId(index.id));
                    }
                    indexes.push(index);
                }
                _ => violations.push(IntegrityViolation::Undecodable {
                    table: Table::Indexes,
                    key,
                }),
            }
        }
        Ok(indexes)
    }

    fn check_index_entries(
        &self,
        indexes: &[Index],
        nodes: &BTreeMap<NodeId, NodeRecord>,
        relationships: &BTreeMap<RelationshipId, RelationshipRecord>,
        violations: &mut Vec<IntegrityViolation>,
    ) -> Result<(), GraphError> {
        let mut expected = BTreeMap::new();
        for index in indexes {
            let mut documents = 0u64;
            let mut length = 0u64;
            let elements = nodes
                .iter()
                .map(|(id, record)| (id.0, Element::Node(record)))
                .chain(
                    relationships
                        .iter()
                        .map(|(id, record)| (id.0, Element::Relationship(record))),
                );
            for (element, record) in elements {
                let Some(entry) = index.definition.entry(record) else {
                    continue;
                };
                if let Entry::Keyword(document) = &entry {
                    documents += 1;
                    length += u64::from(document_length(document));
                }
                for (table, key, value) in index.rows(element, &entry) {
                    expected.insert((table, key), value);
                }
            }
            if matches!(index.definition.kind, IndexKind::Keyword { .. }) {
                expected.insert(
                    (Table::KeywordTotals, keys::index_prefix(index.id).to_vec()),
                    encode_totals(documents, length),
                );
            }
        }
        let mut actual = BTreeMap::new();
        for table in ENTRY_TABLES {
            for (key, value) in self.transaction.scan_prefix(table, &[])? {
                actual.insert((table, key), value);
            }
        }
        for ((table, key), value) in &expected {
            match actual.get(&(*table, key.clone())) {
                None => violations.push(IntegrityViolation::MissingIndexEntry {
                    table: *table,
                    key: key.clone(),
                }),
                Some(stored) if stored != value => {
                    violations.push(IntegrityViolation::WrongIndexEntry {
                        table: *table,
                        key: key.clone(),
                    });
                }
                Some(_) => {}
            }
        }
        for (table, key) in actual.keys() {
            if !expected.contains_key(&(*table, key.clone())) {
                violations.push(IntegrityViolation::UnexpectedIndexEntry {
                    table: *table,
                    key: key.clone(),
                });
            }
        }
        Ok(())
    }

    fn check_counters(
        &self,
        nodes: &BTreeMap<NodeId, NodeRecord>,
        relationships: &BTreeMap<RelationshipId, RelationshipRecord>,
        named: &BTreeSet<(u8, u32)>,
        indexes: &[Index],
        label_sets: &LabelSets,
        violations: &mut Vec<IntegrityViolation>,
    ) -> Result<(), GraphError> {
        let mut check = |counter: Counter, key: &[u8], highest: Option<u64>| {
            let pending = self.counters.iter().find(|(held, _)| held == key);
            let stored = match pending {
                Some((_, next)) => Some(next.to_be_bytes().to_vec()),
                None => self.transaction.get(Table::Metadata, key)?,
            };
            let next = match stored {
                Some(bytes) => match decode_u64(&bytes) {
                    Ok(next) => next,
                    Err(_) => {
                        violations.push(IntegrityViolation::Undecodable {
                            table: Table::Metadata,
                            key: key.to_vec(),
                        });
                        return Ok(());
                    }
                },
                None => 0,
            };
            if let Some(highest) = highest.filter(|highest| next <= *highest) {
                violations.push(IntegrityViolation::CounterNotAboveAllocated { counter, highest });
            }
            Ok::<(), GraphError>(())
        };
        check(
            Counter::NodeId,
            keys::NEXT_NODE_ID_KEY,
            nodes.keys().next_back().map(|id| id.0),
        )?;
        check(
            Counter::RelationshipId,
            keys::NEXT_RELATIONSHIP_ID_KEY,
            relationships.keys().next_back().map(|id| id.0),
        )?;
        check(
            Counter::IndexId,
            keys::NEXT_INDEX_ID_KEY,
            indexes.iter().map(|index| u64::from(index.id)).max(),
        )?;
        check(
            Counter::LabelSetId,
            keys::NEXT_LABEL_SET_KEY,
            label_sets
                .keys()
                .next_back()
                .filter(|set| **set != LabelSetId::EMPTY)
                .map(|set| u64::from(set.0)),
        )?;
        for tag in TOKEN_TAGS {
            let highest = named
                .iter()
                .filter(|(held, _)| *held == tag)
                .map(|(_, id)| u64::from(*id))
                .max();
            check(Counter::Token(tag), &keys::next_token_key(tag), highest)?;
        }
        Ok(())
    }
}

fn canonical(edit: &MetaEdit, label_sets: &LabelSets) -> Result<Option<Vec<u8>>, GraphError> {
    edit.encode(|set| {
        label_sets.get(&set).cloned().ok_or(GraphError::Corrupt(
            "a node has a label set that does not exist",
        ))
    })
}

fn covered_properties(kind: &IndexKind) -> &[Token<PropertyKey>] {
    match kind {
        IndexKind::Keyword { properties } => properties,
        IndexKind::Vector { property, .. } | IndexKind::Property { property } => {
            std::slice::from_ref(property)
        }
    }
}

fn check_properties(properties: &Properties, check_named: &mut impl FnMut(u8, u32)) {
    for key in properties.keys() {
        check_named(PropertyKey::TAG, key.id());
    }
}

#[cfg(test)]
#[path = "integrity.test.rs"]
mod tests;
