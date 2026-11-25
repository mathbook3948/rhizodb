use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Arc;

use crate::storage::ReadTransaction;
use crate::storage::Table;
use crate::value::PropertyValue;

use super::FORMAT_VERSION;
use super::GraphError;
use super::GraphTx;
use super::NodeId;
use super::NodeRecord;
use super::RelationshipId;
use super::RelationshipRecord;
use super::adjacency;
use super::adjacency::Amended;
use super::adjacency::Change;
use super::adjacency::Edge;
use super::adjacency::PartRef;
use super::adjacency::SegmentId;
use super::adjacency::Side;
use super::columns;
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
use super::meta::Delta;
use super::meta::Flush;
use super::meta::LabelSetId;
use super::meta::MetaBytes;
use super::meta::MetaEdit;
use super::meta::Part;
use super::postings;
use super::postings::Block;
use super::properties::ColumnWrite;
use super::records::Properties;
use super::relationships;
use super::relationships::ChunkRef;
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
        side: Side,
        relationship: RelationshipId,
    },
    OrphanAdjacency {
        side: Side,
        node: NodeId,
        relationship: RelationshipId,
    },
    NonCanonicalAdjacency(SegmentId),
    LabelSetTablesDisagree(u32),
    UnknownLabelSet {
        node: NodeId,
        set: u32,
    },
    PropertyOfMissingNode {
        node: NodeId,
        key: u32,
    },
    NonCanonicalColumn {
        group: u64,
        key: u32,
    },
    NonCanonicalColumnDelta(NodeId),
    TooManyColumnDeltas(u64),
    NonCanonicalNodeMeta(u64),
    StrayNodeMetaDelta(NodeId),
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

type Adjacency = BTreeMap<(Side, NodeId, u32, RelationshipId), NodeId>;

impl<T: ReadTransaction> GraphTx<T> {
    /// Checks every invariant the key layout implies and returns all violations found. An empty list means the graph is consistent.
    pub(crate) fn check_integrity(&self) -> Result<Vec<IntegrityViolation>, GraphError> {
        let mut violations = Vec::new();
        self.check_format_version(&mut violations)?;
        let named = self.check_tokens(&mut violations)?;
        let label_sets = self.load_label_sets(&mut violations)?;
        let nodes = self.load_nodes(&label_sets, &mut violations)?;
        let relationships = self.load_relationships(&mut violations)?;
        let adjacency = self.load_adjacency(&mut violations)?;
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
            for property in index.definition.kind.properties() {
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
                (Side::Outgoing, record.source, record.target),
                (Side::Incoming, record.target, record.source),
            ];
            for (side, owner, other) in expected {
                if adjacency.get(&(side, owner, record.rel_type.id(), *id)) != Some(&other) {
                    violations.push(IntegrityViolation::MissingAdjacency {
                        side,
                        relationship: *id,
                    });
                }
            }
        }
        for ((side, node, rel_type, relationship), other) in &adjacency {
            let matches = relationships.get(relationship).is_some_and(|record| {
                let (owner, far) = match side {
                    Side::Outgoing => (record.source, record.target),
                    Side::Incoming => (record.target, record.source),
                };
                owner == *node && far == *other && record.rel_type.id() == *rel_type
            });
            if !matches {
                violations.push(IntegrityViolation::OrphanAdjacency {
                    side: *side,
                    node: *node,
                    relationship: *relationship,
                });
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
        for (set, (key, _)) in &self.new_label_sets {
            by_id.insert(*set, meta::parse_label_set(key)?);
        }
        Ok(by_id)
    }

    fn load_nodes(
        &self,
        label_sets: &LabelSets,
        violations: &mut Vec<IntegrityViolation>,
    ) -> Result<BTreeMap<NodeId, NodeRecord>, GraphError> {
        let mut entries: BTreeMap<Vec<u8>, Vec<u8>> = self
            .transaction
            .scan_prefix(Table::NodeChunks, &[])?
            .into_iter()
            .collect();
        for (group, edit) in &self.meta {
            match edit.clone().flush()? {
                Flush::Rewrite { view, stale } => {
                    let key = meta::meta_key(*group).to_vec();
                    match view.encode(*group, |set| self.label_set(set))? {
                        Some(value) => entries.insert(key, value),
                        None => entries.remove(&key),
                    };
                    for slot in stale {
                        entries.remove(meta::delta_key(*group, slot).as_slice());
                    }
                }
                Flush::Deltas { put, delete } => {
                    for delta in put {
                        entries.insert(
                            meta::delta_key(*group, delta.slot).to_vec(),
                            delta.encode(*group).to_vec(),
                        );
                    }
                    for slot in delete {
                        entries.remove(meta::delta_key(*group, slot).as_slice());
                    }
                }
            }
        }
        let mut groups: BTreeMap<u64, StoredGroup> = BTreeMap::new();
        for (key, value) in entries {
            match meta::parse_key(&key) {
                Ok(Some((group, Part::Chunk))) => {
                    groups.entry(group).or_default().chunk = Some(value);
                }
                Ok(Some((group, Part::Delta(_)))) => {
                    groups.entry(group).or_default().deltas.push((key, value));
                }
                Ok(None) | Err(_) => violations.push(IntegrityViolation::Undecodable {
                    table: Table::NodeChunks,
                    key,
                }),
            }
        }
        let mut live = BTreeMap::new();
        for (group, stored) in groups {
            let Some(edit) = check_group(group, stored, label_sets, violations)? else {
                continue;
            };
            for (slot, set) in edit.live() {
                let node = meta::node_at(group, slot);
                if !label_sets.contains_key(&set) {
                    violations.push(IntegrityViolation::UnknownLabelSet { node, set: set.0 });
                }
                live.insert(node, set);
            }
        }
        let properties = self.load_columns(&live, violations)?;
        let mut nodes = BTreeMap::new();
        for (node, set) in live {
            let labels = label_sets
                .get(&set)
                .map(|labels| labels.iter().copied().collect())
                .unwrap_or_default();
            let properties = properties.get(&node).cloned().unwrap_or_default();
            nodes.insert(node, NodeRecord { labels, properties });
        }
        Ok(nodes)
    }

    fn load_columns(
        &self,
        live: &BTreeMap<NodeId, LabelSetId>,
        violations: &mut Vec<IntegrityViolation>,
    ) -> Result<BTreeMap<NodeId, Properties>, GraphError> {
        let mut entries: BTreeMap<Vec<u8>, Vec<u8>> = self
            .transaction
            .scan_prefix(Table::NodeColumns, &[])?
            .into_iter()
            .collect();
        for (id, edit) in &self.columns {
            for write in self.column_writes(*id, edit.clone())?.0 {
                match write {
                    ColumnWrite::Put(key, value) => {
                        entries.insert(key.to_vec(), value);
                    }
                    ColumnWrite::Delete(key) => {
                        entries.remove(key.as_slice());
                    }
                    ColumnWrite::DeleteDeltas(group) => {
                        let (start, end) = columns::deltas_range(group);
                        let doomed: Vec<Vec<u8>> = entries
                            .range(start.to_vec()..end.to_vec())
                            .map(|(key, _)| key.clone())
                            .collect();
                        for key in doomed {
                            entries.remove(&key);
                        }
                    }
                }
            }
        }
        let mut base: BTreeMap<columns::ColumnId, BTreeMap<usize, PropertyValue>> = BTreeMap::new();
        let mut segments: BTreeMap<(u64, Token<PropertyKey>, usize), StoredSegment> =
            BTreeMap::new();
        let mut deltas: BTreeMap<u64, Vec<StoredDelta>> = BTreeMap::new();
        for (key, value) in entries {
            match columns::parse_key(&key) {
                Ok(columns::ColumnKey::Segment {
                    group,
                    token,
                    segment,
                    ..
                }) => {
                    let stored = segments.entry((group, token, segment)).or_default();
                    match check_part(&key, &value) {
                        Ok(cells) => {
                            let column = base.entry((group, token)).or_default();
                            for (at, value) in &cells {
                                column.insert(segment * columns::SEGMENT_SLOTS + at, value.clone());
                            }
                            stored.cells.extend(cells);
                        }
                        Err(_) => {
                            stored.unreadable = true;
                            violations.push(IntegrityViolation::Undecodable {
                                table: Table::NodeColumns,
                                key: key.clone(),
                            });
                        }
                    }
                    stored.parts.push((key, value));
                }
                Ok(columns::ColumnKey::Delta { group, slot }) => {
                    deltas.entry(group).or_default().push((slot, key, value));
                }
                Err(_) => violations.push(IntegrityViolation::Undecodable {
                    table: Table::NodeColumns,
                    key,
                }),
            }
        }
        for ((group, token, segment), stored) in segments {
            if stored.unreadable {
                continue;
            }
            let cells: Vec<(usize, columns::Cell<'_>)> = stored
                .cells
                .iter()
                .map(|(at, value)| (*at, columns::Cell::of(value)))
                .collect();
            let canonical = columns::encode_segment(group, token, segment, &cells)?
                .into_iter()
                .map(|(key, bytes)| (key.to_vec(), bytes))
                .eq(stored.parts);
            if !canonical {
                violations.push(IntegrityViolation::NonCanonicalColumn {
                    group,
                    key: token.id(),
                });
            }
        }
        let mut changes: Vec<(columns::ColumnId, usize, Option<PropertyValue>)> = Vec::new();
        for (group, stored) in deltas {
            if stored.len() > columns::MAX_DELTAS {
                violations.push(IntegrityViolation::TooManyColumnDeltas(group));
            }
            for (slot, key, bytes) in stored {
                let Ok(entries) = decode_delta(&key, &bytes) else {
                    violations.push(IntegrityViolation::Undecodable {
                        table: Table::NodeColumns,
                        key,
                    });
                    continue;
                };
                let encoded = columns::encode_delta(
                    &columns::delta_key(group, slot),
                    entries
                        .iter()
                        .map(|(token, value)| (*token, value.as_ref().map(columns::Cell::of))),
                );
                let mut canonical = encoded == bytes;
                for (token, after) in entries {
                    let before = base
                        .get(&(group, token))
                        .and_then(|column| column.get(&slot))
                        .map(columns::Cell::of);
                    canonical &=
                        !columns::same_state(before, after.as_ref().map(columns::Cell::of));
                    changes.push(((group, token), slot, after));
                }
                if !canonical {
                    violations.push(IntegrityViolation::NonCanonicalColumnDelta(meta::node_at(
                        group, slot,
                    )));
                }
            }
        }
        for (id, slot, after) in changes {
            let column = base.entry(id).or_default();
            match after {
                Some(value) => column.insert(slot, value),
                None => column.remove(&slot),
            };
        }
        let mut properties: BTreeMap<NodeId, Properties> = BTreeMap::new();
        for ((group, token), column) in base {
            for (slot, value) in column {
                let node = meta::node_at(group, slot);
                if live.contains_key(&node) {
                    properties.entry(node).or_default().insert(token, value);
                } else {
                    violations.push(IntegrityViolation::PropertyOfMissingNode {
                        node,
                        key: token.id(),
                    });
                }
            }
        }
        Ok(properties)
    }

    fn load_relationships(
        &self,
        violations: &mut Vec<IntegrityViolation>,
    ) -> Result<BTreeMap<RelationshipId, RelationshipRecord>, GraphError> {
        let mut relationships = BTreeMap::new();
        for (key, value) in self
            .transaction
            .scan_prefix(Table::RelationshipChunks, &[])?
        {
            let chunk = match relationships::parse_key(&key) {
                Ok(chunk) if !self.relationship_chunks.contains_key(&chunk) => chunk,
                Ok(_) => continue,
                Err(_) => {
                    violations.push(IntegrityViolation::Undecodable {
                        table: Table::RelationshipChunks,
                        key,
                    });
                    continue;
                }
            };
            let decoded: Result<Vec<_>, GraphError> = ChunkRef::parse(&value).and_then(|stored| {
                stored
                    .entries()
                    .map(|entry| {
                        let (slot, record) = entry?;
                        Ok((
                            relationships::relationship_at(chunk, slot),
                            RelationshipRecord::decode(record)?,
                        ))
                    })
                    .collect()
            });
            match decoded {
                Ok(records) => relationships.extend(records),
                Err(_) => violations.push(IntegrityViolation::Undecodable {
                    table: Table::RelationshipChunks,
                    key,
                }),
            }
        }
        for (&chunk, edit) in &self.relationship_chunks {
            for (slot, record) in edit.entries() {
                relationships.insert(
                    relationships::relationship_at(chunk, slot),
                    RelationshipRecord::decode(record)?,
                );
            }
        }
        Ok(relationships)
    }

    fn load_adjacency(
        &self,
        violations: &mut Vec<IntegrityViolation>,
    ) -> Result<Adjacency, GraphError> {
        let mut segments: BTreeMap<SegmentId, Vec<adjacency::Stored>> = BTreeMap::new();
        for (key, value) in self.transaction.scan_prefix(Table::Adjacency, &[])? {
            match adjacency::parse_key(&key) {
                Ok((id, _)) => segments.entry(id).or_default().push((key, value)),
                Err(_) => violations.push(IntegrityViolation::Undecodable {
                    table: Table::Adjacency,
                    key,
                }),
            }
        }
        let mut entries = Adjacency::new();
        for (id, stored) in segments {
            let checked = check_segment(&id, &stored, violations)?;
            if self.adjacency.contains_key(&id) {
                continue;
            }
            let Some(slots) = checked else {
                continue;
            };
            for (position, edges) in slots {
                for edge in edges {
                    entries.insert(
                        (
                            id.side,
                            id.node_at(position),
                            id.rel_type,
                            edge.relationship,
                        ),
                        edge.other,
                    );
                }
            }
        }
        for (id, edit) in &self.adjacency {
            for (position, edges) in edit.slots()? {
                for edge in edges {
                    entries.insert(
                        (
                            id.side,
                            id.node_at(position),
                            id.rel_type,
                            edge.relationship,
                        ),
                        edge.other,
                    );
                }
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
            let mut blocks: BTreeMap<Vec<u8>, Block> = BTreeMap::new();
            for (element, record) in elements {
                let Some(entry) = index.definition.entry(record) else {
                    continue;
                };
                if let Entry::Keyword(document) = &entry {
                    documents += 1;
                    let terms = document_length(document);
                    length += u64::from(terms);
                    let block = postings::block_of(element);
                    for (term, positions) in document {
                        blocks
                            .entry(keys::posting_key(index.id, term, block))
                            .or_default()
                            .insert(block, element, positions, terms)?;
                    }
                }
                for (table, key, value) in index.rows(element, &entry) {
                    expected.insert((table, key), value);
                }
            }
            for (key, block) in blocks {
                if let Some(bytes) = block.bytes() {
                    expected.insert((Table::KeywordPostings, key), bytes.to_vec());
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
        for (key, block) in self.postings.under(&[]) {
            let at = (Table::KeywordPostings, key);
            match block.bytes() {
                Some(bytes) => actual.insert(at, bytes.to_vec()),
                None => actual.remove(&at),
            };
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

#[derive(Default)]
struct StoredGroup {
    chunk: Option<Vec<u8>>,
    deltas: Vec<(Vec<u8>, Vec<u8>)>,
}

fn check_group(
    group: u64,
    stored: StoredGroup,
    label_sets: &LabelSets,
    violations: &mut Vec<IntegrityViolation>,
) -> Result<Option<MetaEdit>, GraphError> {
    let undecodable = |key: Vec<u8>| IntegrityViolation::Undecodable {
        table: Table::NodeChunks,
        key,
    };
    let Some(chunk) = stored.chunk else {
        for (key, value) in stored.deltas {
            match Delta::parse(&key, &value) {
                Ok(delta) => violations.push(IntegrityViolation::StrayNodeMetaDelta(
                    meta::node_at(group, delta.slot),
                )),
                Err(_) => violations.push(undecodable(key)),
            }
        }
        return Ok(None);
    };
    let Some(mut edit) = MetaBytes::parse(group, &chunk)
        .ok()
        .and_then(|bytes| bytes.to_edit().ok())
    else {
        violations.push(undecodable(meta::meta_key(group).to_vec()));
        return Ok(None);
    };
    let known = edit.live().all(|(_, set)| label_sets.contains_key(&set));
    let mut canonical = !known || encode(group, &edit, label_sets)? == Some(chunk);
    canonical &= stored.deltas.len() <= meta::MAX_DELTAS;
    for (key, value) in stored.deltas {
        let Ok(delta) = Delta::parse(&key, &value) else {
            violations.push(undecodable(key));
            continue;
        };
        if edit.label_set(delta.slot) != delta.before {
            violations.push(IntegrityViolation::StrayNodeMetaDelta(meta::node_at(
                group, delta.slot,
            )));
            continue;
        }
        canonical &= delta.before != delta.after;
        edit.set(delta.slot, delta.after);
    }
    canonical &= !edit.is_empty();
    if !canonical {
        violations.push(IntegrityViolation::NonCanonicalNodeMeta(group));
    }
    Ok(Some(edit))
}

type StoredDelta = (usize, Vec<u8>, Vec<u8>);

type DeltaEntry = (Token<PropertyKey>, Option<PropertyValue>);

#[derive(Default)]
struct StoredSegment {
    parts: Vec<(Vec<u8>, Vec<u8>)>,
    cells: Vec<(usize, PropertyValue)>,
    unreadable: bool,
}

fn check_part(key: &[u8], bytes: &[u8]) -> Result<Vec<(usize, PropertyValue)>, GraphError> {
    columns::SegmentRef::parse(key, bytes)?
        .cells()
        .map(|cell| {
            let (at, cell) = cell?;
            Ok((at, cell.to_value()?))
        })
        .collect()
}

fn decode_delta(key: &[u8], bytes: &[u8]) -> Result<Vec<DeltaEntry>, GraphError> {
    columns::DeltaRef::parse(key, bytes)?
        .entries()
        .map(|entry| {
            let (token, cell) = entry?;
            Ok((token, cell.map(columns::Cell::to_value).transpose()?))
        })
        .collect()
}

fn encode(
    group: u64,
    edit: &MetaEdit,
    label_sets: &LabelSets,
) -> Result<Option<Vec<u8>>, GraphError> {
    edit.encode(group, |set| {
        label_sets.get(&set).cloned().ok_or(GraphError::Corrupt(
            "a node has a label set that does not exist",
        ))
    })
}

fn check_segment(
    id: &SegmentId,
    stored: &[adjacency::Stored],
    violations: &mut Vec<IntegrityViolation>,
) -> Result<Option<adjacency::Slots>, GraphError> {
    let undecodable = |key: &[u8]| IntegrityViolation::Undecodable {
        table: Table::Adjacency,
        key: key.to_vec(),
    };
    let mut parts = BTreeMap::new();
    let mut changes = BTreeMap::new();
    let mut readable = true;
    for (key, value) in stored {
        match adjacency::parse_key(key)? {
            (_, adjacency::Entry::Delta(position)) => match Change::parse(key, value) {
                Ok(change) => {
                    changes.insert(position, (change, value));
                }
                Err(_) => {
                    violations.push(undecodable(key));
                    readable = false;
                }
            },
            (_, adjacency::Entry::Part { .. }) => {
                match PartRef::parse(key, value).and_then(|part| part.slots()) {
                    Ok(slots) => parts.extend(slots),
                    Err(_) => {
                        violations.push(undecodable(key));
                        readable = false;
                    }
                }
            }
        }
    }
    if !readable {
        return Ok(None);
    }
    let held: Vec<(usize, &[Edge])> = parts
        .iter()
        .map(|(position, edges)| (*position, edges.as_slice()))
        .collect();
    let encoded = adjacency::encode_segment(id, &held)?;
    let stored_parts: Vec<adjacency::Stored> = stored
        .iter()
        .filter(|(key, _)| {
            matches!(
                adjacency::parse_key(key),
                Ok((_, adjacency::Entry::Part { .. }))
            )
        })
        .cloned()
        .collect();
    let mut canonical = encoded == stored_parts && changes.len() <= adjacency::MAX_DELTAS;
    canonical &= changes.is_empty() || !stored_parts.is_empty();
    let mut slots = parts;
    for (position, (change, bytes)) in changes {
        canonical &= change.encode(&id.delta_key(position)) == *bytes;
        let stored = slots.remove(&position).unwrap_or_default();
        let mut amended = Amended::new(Some(&change));
        let mut edges = Vec::new();
        let mut agrees = true;
        for edge in stored {
            agrees &= amended.stored(edge, &mut |edge| edges.push(edge)).is_ok();
        }
        agrees &= amended.finish(&mut |edge| edges.push(edge)).is_ok();
        if !agrees {
            violations.push(undecodable(&id.delta_key(position)));
            return Ok(None);
        }
        if !edges.is_empty() {
            slots.insert(position, edges);
        }
    }
    canonical &= !slots.is_empty();
    if !canonical {
        violations.push(IntegrityViolation::NonCanonicalAdjacency(*id));
    }
    Ok(Some(slots.into_iter().collect()))
}

fn check_properties(properties: &Properties, check_named: &mut impl FnMut(u8, u32)) {
    for key in properties.keys() {
        check_named(PropertyKey::TAG, key.id());
    }
}

#[cfg(test)]
#[path = "integrity.test.rs"]
mod tests;
