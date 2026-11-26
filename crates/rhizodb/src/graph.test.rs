use super::records::Properties;
use super::*;
use crate::storage::redb_backend::RedbStorage;

fn memory_graph() -> Graph<RedbStorage> {
    Graph::open(RedbStorage::in_memory().unwrap()).unwrap()
}

fn node_with(labels: &[Token<Label>]) -> NodeRecord {
    NodeRecord {
        labels: labels.iter().copied().collect(),
        properties: Properties::new(),
    }
}

fn relationship(
    rel_type: Token<RelationshipType>,
    source: NodeId,
    target: NodeId,
) -> RelationshipRecord {
    RelationshipRecord {
        rel_type,
        source,
        target,
        properties: Properties::new(),
    }
}

fn others(adjacent: &[Adjacent]) -> Vec<NodeId> {
    adjacent.iter().map(|entry| entry.other).collect()
}

#[test]
fn a_created_node_reads_back_with_its_labels_and_properties() {
    let graph = memory_graph();
    let mut write = graph.write().unwrap();
    let person: Token<Label> = write.intern_token("Person").unwrap();
    let name: Token<PropertyKey> = write.intern_token("name").unwrap();
    let mut record = node_with(&[person]);
    record
        .properties
        .insert(name, PropertyValue::String("Ada".to_owned()));
    let id = write.create_node(record.clone()).unwrap();
    write.commit().unwrap();

    let read = graph.read().unwrap();
    assert_eq!(read.node(id).unwrap(), Some(record));
    assert_eq!(read.node(NodeId(id.0 + 1)).unwrap(), None);
}

#[test]
fn a_created_relationship_reads_back_and_requires_existing_endpoints() {
    let graph = memory_graph();
    let mut write = graph.write().unwrap();
    let knows: Token<RelationshipType> = write.intern_token("KNOWS").unwrap();
    let a = write.create_node(node_with(&[])).unwrap();
    let b = write.create_node(node_with(&[])).unwrap();
    let record = relationship(knows, a, b);
    let id = write.create_relationship(&record).unwrap();

    assert_eq!(write.relationship(id).unwrap(), Some(record));
    assert!(matches!(
        write.create_relationship(&relationship(knows, a, NodeId(99))),
        Err(GraphError::NodeNotFound(NodeId(99)))
    ));
    assert!(matches!(
        write.create_relationship(&relationship(knows, NodeId(98), b)),
        Err(GraphError::NodeNotFound(NodeId(98)))
    ));
}

#[test]
fn adjacency_is_visible_from_both_ends_and_filters_by_type() {
    let graph = memory_graph();
    let mut write = graph.write().unwrap();
    let knows: Token<RelationshipType> = write.intern_token("KNOWS").unwrap();
    let likes: Token<RelationshipType> = write.intern_token("LIKES").unwrap();
    let a = write.create_node(node_with(&[])).unwrap();
    let b = write.create_node(node_with(&[])).unwrap();
    let c = write.create_node(node_with(&[])).unwrap();
    let a_knows_b = write
        .create_relationship(&relationship(knows, a, b))
        .unwrap();
    write
        .create_relationship(&relationship(likes, a, c))
        .unwrap();
    write
        .create_relationship(&relationship(knows, c, a))
        .unwrap();
    write.commit().unwrap();

    let read = graph.read().unwrap();
    let outgoing = read.adjacent(a, Direction::Outgoing, None).unwrap();
    assert_eq!(others(&outgoing), vec![b, c]);
    assert_eq!(outgoing[0].relationship, a_knows_b);
    assert_eq!(outgoing[0].rel_type, knows);

    let incoming = read.adjacent(a, Direction::Incoming, None).unwrap();
    assert_eq!(others(&incoming), vec![c]);

    let both = read.adjacent(a, Direction::Both, None).unwrap();
    assert_eq!(others(&both), vec![b, c, c]);

    let only_knows = read.adjacent(a, Direction::Both, Some(knows)).unwrap();
    assert_eq!(others(&only_knows), vec![b, c]);
    let only_likes = read.adjacent(a, Direction::Outgoing, Some(likes)).unwrap();
    assert_eq!(others(&only_likes), vec![c]);
    assert!(
        read.adjacent(b, Direction::Outgoing, None)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        others(&read.adjacent(b, Direction::Incoming, None).unwrap()),
        vec![a]
    );
}

#[test]
fn a_self_loop_is_reported_once_when_scanning_both_directions() {
    let graph = memory_graph();
    let mut write = graph.write().unwrap();
    let knows: Token<RelationshipType> = write.intern_token("KNOWS").unwrap();
    let a = write.create_node(node_with(&[])).unwrap();
    write
        .create_relationship(&relationship(knows, a, a))
        .unwrap();

    assert_eq!(write.adjacent(a, Direction::Both, None).unwrap().len(), 1);
    assert_eq!(
        write.adjacent(a, Direction::Outgoing, None).unwrap().len(),
        1
    );
    assert_eq!(
        write.adjacent(a, Direction::Incoming, None).unwrap().len(),
        1
    );
}

#[test]
fn the_label_index_follows_label_changes() {
    let graph = memory_graph();
    let mut write = graph.write().unwrap();
    let person: Token<Label> = write.intern_token("Person").unwrap();
    let admin: Token<Label> = write.intern_token("Admin").unwrap();
    let a = write.create_node(node_with(&[person])).unwrap();
    let b = write.create_node(node_with(&[])).unwrap();
    let c = write.create_node(node_with(&[person, admin])).unwrap();
    write.add_label(b, person).unwrap();
    write.add_label(b, person).unwrap();
    write.remove_label(c, person).unwrap();
    write.remove_label(c, person).unwrap();

    assert_eq!(write.node_ids_with_label(person).unwrap(), vec![a, b]);
    assert_eq!(write.node_ids_with_label(admin).unwrap(), vec![c]);
    assert_eq!(
        write.node(c).unwrap().map(|record| record.labels),
        Some([admin].into_iter().collect())
    );
    assert_eq!(write.node_ids().unwrap(), vec![a, b, c]);

    write.delete_node(a).unwrap();
    assert_eq!(write.node_ids_with_label(person).unwrap(), vec![b]);
}

#[test]
fn a_node_with_relationships_is_not_detached_until_they_are_gone() {
    let graph = memory_graph();
    let mut write = graph.write().unwrap();
    let knows: Token<RelationshipType> = write.intern_token("KNOWS").unwrap();
    let a = write.create_node(node_with(&[])).unwrap();
    let b = write.create_node(node_with(&[])).unwrap();
    let edge = write
        .create_relationship(&relationship(knows, a, b))
        .unwrap();

    assert!(matches!(
        write.ensure_detached(a),
        Err(GraphError::NodeHasRelationships(_))
    ));
    assert!(matches!(
        write.ensure_detached(b),
        Err(GraphError::NodeHasRelationships(_))
    ));

    write.delete_relationship(edge).unwrap();
    write.ensure_detached(a).unwrap();
    write.ensure_detached(b).unwrap();
    assert_eq!(write.relationship(edge).unwrap(), None);
    assert!(write.adjacent(a, Direction::Both, None).unwrap().is_empty());
    assert!(write.adjacent(b, Direction::Both, None).unwrap().is_empty());
    write.delete_node(a).unwrap();
    write.delete_node(b).unwrap();
    assert_eq!(write.node_ids().unwrap(), Vec::new());
    assert_eq!(write.delete_node(a).unwrap(), None);
    assert_eq!(write.delete_relationship(edge).unwrap(), None);
}

#[test]
fn properties_can_be_set_replaced_and_removed_on_nodes_and_relationships() {
    let graph = memory_graph();
    let mut write = graph.write().unwrap();
    let knows: Token<RelationshipType> = write.intern_token("KNOWS").unwrap();
    let since: Token<PropertyKey> = write.intern_token("since").unwrap();
    let a = write.create_node(node_with(&[])).unwrap();
    let b = write.create_node(node_with(&[])).unwrap();
    let edge = write
        .create_relationship(&relationship(knows, a, b))
        .unwrap();

    write
        .set_node_property(a, since, PropertyValue::Integer(1))
        .unwrap();
    write
        .set_node_property(a, since, PropertyValue::Integer(2))
        .unwrap();
    write
        .set_relationship_property(edge, since, PropertyValue::Boolean(true))
        .unwrap();
    assert_eq!(
        write.node(a).unwrap().map(|record| record.properties),
        Some(Properties::from([(since, PropertyValue::Integer(2))]))
    );
    assert_eq!(
        write
            .relationship(edge)
            .unwrap()
            .map(|record| record.properties),
        Some(Properties::from([(since, PropertyValue::Boolean(true))]))
    );

    write.remove_node_property(a, since).unwrap();
    write.remove_relationship_property(edge, since).unwrap();
    assert_eq!(write.node(a).unwrap(), Some(node_with(&[])));
    assert_eq!(
        write.relationship(edge).unwrap(),
        Some(relationship(knows, a, b))
    );
    assert!(matches!(
        write.set_node_property(NodeId(77), since, PropertyValue::Integer(1)),
        Err(GraphError::NodeNotFound(_))
    ));
}

#[test]
fn tokens_are_interned_once_per_kind_and_resolve_back_to_names() {
    let graph = memory_graph();
    let mut write = graph.write().unwrap();
    let first: Token<Label> = write.intern_token("name").unwrap();
    let again: Token<Label> = write.intern_token("name").unwrap();
    let other: Token<Label> = write.intern_token("other").unwrap();
    let as_key: Token<PropertyKey> = write.intern_token("name").unwrap();
    write.commit().unwrap();

    assert_eq!(first, again);
    assert_ne!(first, other);
    assert_eq!(as_key.id(), first.id());
    let read = graph.read().unwrap();
    assert_eq!(read.token_name(other).unwrap(), "other");
    assert_eq!(read.token_name(as_key).unwrap(), "name");
    assert_eq!(read.lookup_token::<Label>("other").unwrap(), Some(other));
    assert_eq!(
        read.lookup_token::<RelationshipType>("other").unwrap(),
        None
    );
}

#[test]
fn reading_never_creates_tokens() {
    let graph = memory_graph();
    {
        let read = graph.read().unwrap();
        assert_eq!(read.lookup_token::<Label>("Person").unwrap(), None);
        assert_eq!(read.lookup_token::<PropertyKey>("name").unwrap(), None);
    }

    let raw = graph.storage.begin_read().unwrap();
    assert!(raw.scan_prefix(Table::TokenByName, &[]).unwrap().is_empty());
    assert!(raw.scan_prefix(Table::TokenById, &[]).unwrap().is_empty());
    let mut write = graph.write().unwrap();
    assert_eq!(write.intern_token::<Label>("Person").unwrap().id(), 0);
}

#[test]
fn a_dropped_write_transaction_leaves_no_trace() {
    let graph = memory_graph();
    {
        let mut write = graph.write().unwrap();
        write.create_node(node_with(&[])).unwrap();
    }

    let mut write = graph.write().unwrap();
    assert_eq!(write.node_ids().unwrap(), Vec::new());
    assert_eq!(write.create_node(node_with(&[])).unwrap(), NodeId(0));
}

#[test]
fn a_file_database_keeps_its_graph_across_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph.redb");
    let (person, a, b, edge) = {
        let graph = Graph::open(RedbStorage::open(&path).unwrap()).unwrap();
        let mut write = graph.write().unwrap();
        let person: Token<Label> = write.intern_token("Person").unwrap();
        let knows: Token<RelationshipType> = write.intern_token("KNOWS").unwrap();
        let a = write.create_node(node_with(&[person])).unwrap();
        let b = write.create_node(node_with(&[person])).unwrap();
        let edge = write
            .create_relationship(&relationship(knows, a, b))
            .unwrap();
        write.commit().unwrap();
        (person, a, b, edge)
    };

    let graph = Graph::open(RedbStorage::open(&path).unwrap()).unwrap();
    let read = graph.read().unwrap();
    assert_eq!(read.lookup_token::<Label>("Person").unwrap(), Some(person));
    assert_eq!(read.node_ids_with_label(person).unwrap(), vec![a, b]);
    assert_eq!(
        others(&read.adjacent(a, Direction::Outgoing, None).unwrap()),
        vec![b]
    );
    assert_eq!(read.relationship(edge).unwrap().map(|r| r.target), Some(b));
}

#[test]
fn identifiers_are_not_reused_after_deletion_and_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph.redb");
    let (last_node, last_edge) = {
        let graph = Graph::open(RedbStorage::open(&path).unwrap()).unwrap();
        let mut write = graph.write().unwrap();
        let knows: Token<RelationshipType> = write.intern_token("KNOWS").unwrap();
        let a = write.create_node(node_with(&[])).unwrap();
        let b = write.create_node(node_with(&[])).unwrap();
        let edge = write
            .create_relationship(&relationship(knows, a, b))
            .unwrap();
        write.delete_relationship(edge).unwrap();
        write.delete_node(b).unwrap();
        write.commit().unwrap();
        (b, edge)
    };

    let graph = Graph::open(RedbStorage::open(&path).unwrap()).unwrap();
    let mut write = graph.write().unwrap();
    let knows: Token<RelationshipType> = write.intern_token("KNOWS").unwrap();
    let fresh = write.create_node(node_with(&[])).unwrap();
    let edge = write
        .create_relationship(&relationship(knows, fresh, fresh))
        .unwrap();
    assert!(fresh > last_node);
    assert!(edge > last_edge);
}

#[test]
fn an_unknown_format_version_is_rejected_on_open() {
    let storage = RedbStorage::in_memory().unwrap();
    let mut write = storage.begin_write().unwrap();
    write
        .put(
            Table::Metadata,
            keys::FORMAT_VERSION_KEY,
            &99u32.to_be_bytes(),
        )
        .unwrap();
    write.commit().unwrap();

    assert!(matches!(
        Graph::open(storage),
        Err(GraphError::UnsupportedFormatVersion(99))
    ));
}

#[test]
fn the_current_format_version_is_written_on_first_open_and_accepted_after() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph.redb");
    Graph::open(RedbStorage::open(&path).unwrap()).unwrap();

    let storage = RedbStorage::open(&path).unwrap();
    let stored = storage
        .begin_read()
        .unwrap()
        .get(Table::Metadata, keys::FORMAT_VERSION_KEY)
        .unwrap();
    assert_eq!(stored, Some(FORMAT_VERSION.to_be_bytes().to_vec()));
    assert!(Graph::open(storage).is_ok());
}

#[test]
fn a_version_one_database_opens_and_is_upgraded_to_the_current_version() {
    let storage = RedbStorage::in_memory().unwrap();
    let mut write = storage.begin_write().unwrap();
    write
        .put(
            Table::Metadata,
            keys::FORMAT_VERSION_KEY,
            &1u32.to_be_bytes(),
        )
        .unwrap();
    write.commit().unwrap();

    let graph = Graph::open(storage).unwrap();
    let stored = graph
        .storage
        .begin_read()
        .unwrap()
        .get(Table::Metadata, keys::FORMAT_VERSION_KEY)
        .unwrap();
    assert_eq!(stored, Some(FORMAT_VERSION.to_be_bytes().to_vec()));
}

#[test]
fn a_version_six_database_reads_its_float_lists_after_the_upgrade() {
    let storage = RedbStorage::in_memory().unwrap();
    let mut write = storage.begin_write().unwrap();
    write
        .put(
            Table::Metadata,
            keys::FORMAT_VERSION_KEY,
            &6u32.to_be_bytes(),
        )
        .unwrap();
    let mut record = vec![0, 2, 0, 0, 0, 0, 5, 2, 2];
    record.extend_from_slice(&0.5f64.to_bits().to_be_bytes());
    record.push(2);
    record.extend_from_slice(&(-1.0f64).to_bits().to_be_bytes());
    record.extend_from_slice(&[0, 0, 0, 1, 1]);
    record.extend_from_slice(&7i64.to_be_bytes());
    write
        .put(Table::Nodes, &keys::element_key(0), &record)
        .unwrap();
    write.commit().unwrap();

    let graph = Graph::open(storage).unwrap();
    let read = graph.read().unwrap();
    let node = read.node(NodeId(0)).unwrap().unwrap();
    assert_eq!(
        node.properties.get(&Token::new(0)),
        Some(&PropertyValue::List(vec![
            PropertyValue::Float(0.5),
            PropertyValue::Float(-1.0)
        ]))
    );
    assert_eq!(
        node.properties.get(&Token::new(1)),
        Some(&PropertyValue::Integer(7))
    );
}

#[test]
fn a_property_after_a_list_of_single_precision_floats_reads_back() {
    let graph = memory_graph();
    let mut write = graph.write().unwrap();
    let embedding: Token<PropertyKey> = write.intern_token("embedding").unwrap();
    let age: Token<PropertyKey> = write.intern_token("age").unwrap();
    let mut record = node_with(&[]);
    let floats = (0..64).map(|i| PropertyValue::Float(f64::from(i) / 64.0));
    record
        .properties
        .insert(embedding, PropertyValue::List(floats.collect()));
    record.properties.insert(age, PropertyValue::Integer(41));
    let node = write.create_node(record.clone()).unwrap();
    write.commit().unwrap();

    let read = graph.read().unwrap();
    assert_eq!(read.node(node).unwrap(), Some(record));
    let found = read
        .node_property(
            &mut LabelReader::default(),
            &mut ColumnReader::default(),
            node,
            Some(age),
        )
        .unwrap();
    assert_eq!(found, Some(Some(PropertyValue::Integer(41))));
}

#[test]
fn a_newer_format_version_is_rejected_on_open() {
    let storage = RedbStorage::in_memory().unwrap();
    let mut write = storage.begin_write().unwrap();
    let newer = FORMAT_VERSION + 1;
    write
        .put(
            Table::Metadata,
            keys::FORMAT_VERSION_KEY,
            &newer.to_be_bytes(),
        )
        .unwrap();
    write.commit().unwrap();

    assert!(matches!(
        Graph::open(storage),
        Err(GraphError::UnsupportedFormatVersion(found)) if found == newer
    ));
}

#[test]
fn a_database_of_format_seven_moves_its_labels_to_node_meta_and_passes_the_check() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph.redb");
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("format-7.redb");
    std::fs::copy(fixture, &path).unwrap();
    let before = RedbStorage::open(&path).unwrap();
    let read = before.begin_read().unwrap();
    assert_eq!(
        read.get(Table::Metadata, keys::FORMAT_VERSION_KEY).unwrap(),
        Some(7u32.to_be_bytes().to_vec())
    );
    assert!(!read.scan_prefix(Table::LabelIndex, &[]).unwrap().is_empty());
    drop(read);

    let graph = Graph::open(before).unwrap();
    let read = graph.read().unwrap();
    assert_eq!(read.check_integrity().unwrap(), vec![]);
    assert_eq!(
        read.transaction
            .scan_prefix(Table::LabelIndex, &[])
            .unwrap(),
        Vec::new()
    );
    let person = read.lookup_token::<Label>("Person").unwrap().unwrap();
    let people = read.node_ids_with_label(person).unwrap();
    assert_eq!(people.len(), 6);
    assert_eq!(read.count_nodes_with_label(person).unwrap(), 6);
    assert_eq!(people.last(), Some(&NodeId(4209)));
}

#[test]
fn a_label_scan_and_count_see_the_writes_of_their_own_transaction_in_id_order() {
    let graph = memory_graph();
    let mut write = graph.write().unwrap();
    let person: Token<Label> = write.intern_token("Person").unwrap();
    let mut ids = Vec::new();
    for _ in 0..5000 {
        ids.push(write.create_node(node_with(&[person])).unwrap());
    }
    write.commit().unwrap();
    let mut write = graph.write().unwrap();
    write.delete_node(ids[10]).unwrap();
    write.remove_label(ids[4500], person).unwrap();
    let late = write.create_node(node_with(&[person])).unwrap();
    let mut expected: Vec<NodeId> = ids
        .iter()
        .copied()
        .filter(|id| *id != ids[10] && *id != ids[4500])
        .collect();
    expected.push(late);
    assert_eq!(write.node_ids_with_label(person).unwrap(), expected);
    assert_eq!(write.count_nodes_with_label(person).unwrap(), 4999);
    assert_eq!(write.node_ids().unwrap().len(), 5000);
    write.commit().unwrap();
    let read = graph.read().unwrap();
    assert_eq!(read.node_ids_with_label(person).unwrap(), expected);
    assert_eq!(read.count_nodes_with_label(person).unwrap(), 4999);
    assert_eq!(read.check_integrity().unwrap(), vec![]);
}

#[test]
fn a_write_transaction_that_outgrows_its_held_chunks_still_reads_and_commits_its_relationships() {
    let graph = memory_graph();
    let mut write = graph.write().unwrap();
    let rel_type = write.intern_token::<RelationshipType>("BIG").unwrap();
    let key = write.intern_token::<PropertyKey>("text").unwrap();
    let a = write.create_node(node_with(&[])).unwrap();
    let b = write.create_node(node_with(&[])).unwrap();
    let mut expected = BTreeMap::new();
    let mut held = 0;
    for index in 0..400usize {
        let mut record = relationship(rel_type, a, b);
        record.properties.insert(
            key,
            PropertyValue::String(format!("{index}").repeat(16 << 10)),
        );
        held += record.encode().len();
        let id = write.create_relationship(&record).unwrap();
        expected.insert(id, record);
    }
    assert!(held > 2 * HELD_RELATIONSHIP_BYTES);
    let ids: Vec<RelationshipId> = expected.keys().copied().collect();
    for (at, id) in ids.into_iter().enumerate() {
        if at % 3 == 0 {
            write.delete_relationship(id).unwrap();
            expected.remove(&id);
        }
    }
    for (id, record) in &expected {
        assert_eq!(write.relationship(*id).unwrap().as_ref(), Some(record));
    }
    let mut visited = Vec::new();
    write
        .visit_relationship_records(|id, _| {
            visited.push(id);
            Ok(())
        })
        .unwrap();
    assert_eq!(visited, expected.keys().copied().collect::<Vec<_>>());
    assert_eq!(write.check_integrity().unwrap(), vec![]);
    write.commit().unwrap();
    let read = graph.read().unwrap();
    for (id, record) in &expected {
        assert_eq!(read.relationship(*id).unwrap().as_ref(), Some(record));
    }
    assert_eq!(read.check_integrity().unwrap(), vec![]);
}

#[test]
fn a_database_of_format_eleven_moves_every_relationship_record_into_its_chunk() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph.redb");
    let mut expected = BTreeMap::new();
    {
        let graph = Graph::open(RedbStorage::open(&path).unwrap()).unwrap();
        let mut write = graph.write().unwrap();
        let types = [
            write.intern_token::<RelationshipType>("A").unwrap(),
            write.intern_token::<RelationshipType>("B").unwrap(),
        ];
        let key = write.intern_token::<PropertyKey>("k").unwrap();
        let nodes: Vec<NodeId> = (0..300)
            .map(|_| write.create_node(node_with(&[])).unwrap())
            .collect();
        for index in 0..70_000usize {
            let mut record = relationship(
                types[index % 2],
                nodes[index % nodes.len()],
                nodes[index * 7 % nodes.len()],
            );
            if index % 5 != 0 {
                record
                    .properties
                    .insert(key, PropertyValue::Integer(index as i64));
            }
            let id = write.create_relationship(&record).unwrap();
            if index % 9 == 4 || (40..90).contains(&index) {
                write.delete_relationship(id).unwrap();
            } else {
                expected.insert(id, record);
            }
        }
        write.commit().unwrap();
        let mut previous = graph.storage.begin_write().unwrap();
        store_as_format_eleven(&mut previous);
        previous.commit().unwrap();
    }
    assert!(expected.keys().last().unwrap().0 >= 1 << 16);
    for _ in 0..2 {
        let graph = Graph::open(RedbStorage::open(&path).unwrap()).unwrap();
        let read = graph.read().unwrap();
        for (id, record) in &expected {
            assert_eq!(read.relationship(*id).unwrap().as_ref(), Some(record));
        }
        let mut count = 0;
        read.visit_relationship_records(|_, _| {
            count += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(count, expected.len());
        assert_eq!(
            read.transaction
                .scan_prefix(Table::Relationships, &[])
                .unwrap(),
            vec![]
        );
        assert_eq!(read.check_integrity().unwrap(), vec![]);
    }
}

/// Deletes every property column and returns the properties they held, by node.
pub(crate) fn take_columns(write: &mut impl WriteTransaction) -> BTreeMap<NodeId, Properties> {
    let mut values: BTreeMap<(NodeId, Token<PropertyKey>), PropertyValue> = BTreeMap::new();
    let mut changes = Vec::new();
    for (key, value) in write.scan_prefix(Table::NodeColumns, &[]).unwrap() {
        match columns::parse_key(&key).unwrap() {
            columns::ColumnKey::Segment {
                group,
                token,
                segment,
                ..
            } => {
                let view = columns::SegmentRef::parse(&key, &value).unwrap();
                for cell in view.cells() {
                    let (at, cell) = cell.unwrap();
                    let node = meta::node_at(group, segment * columns::SEGMENT_SLOTS + at);
                    values.insert((node, token), cell.to_value().unwrap());
                }
            }
            columns::ColumnKey::Delta { group, slot } => {
                let delta = columns::DeltaRef::parse(&key, &value).unwrap();
                for entry in delta.entries() {
                    let (token, cell) = entry.unwrap();
                    let value = cell.map(|cell| cell.to_value().unwrap());
                    changes.push(((meta::node_at(group, slot), token), value));
                }
            }
        }
        write.delete(Table::NodeColumns, &key).unwrap();
    }
    for (at, value) in changes {
        match value {
            Some(value) => values.insert(at, value),
            None => values.remove(&at),
        };
    }
    let mut nodes: BTreeMap<NodeId, Properties> = BTreeMap::new();
    for ((node, token), value) in values {
        nodes.entry(node).or_default().insert(token, value);
    }
    nodes
}

/// Rewrites a database of the current format in the layout of format 13, which kept each
/// element's posting of a term as its own entry under the element's ID, so a test can open it as
/// an older version.
pub(crate) fn store_as_format_thirteen(write: &mut impl WriteTransaction) {
    let blocks = write.scan_prefix(Table::KeywordPostings, &[]).unwrap();
    for (key, _) in &blocks {
        write.delete(Table::KeywordPostings, key).unwrap();
    }
    for (key, value) in blocks {
        let block = keys::parse_posting_block(&key).unwrap();
        let prefix = &key[..key.len() - 8];
        for posting in postings::Entries::new(block, &value) {
            let posting = posting.unwrap();
            let mut row = posting.frequency.to_be_bytes().to_vec();
            row.extend_from_slice(&posting.length.to_be_bytes());
            let mut previous = 0;
            for position in &posting.positions {
                crate::value::put_varint(&mut row, usize::try_from(position - previous).unwrap());
                previous = *position;
            }
            let mut element_key = prefix.to_vec();
            element_key.extend_from_slice(&posting.element.to_be_bytes());
            write
                .put(Table::KeywordPostings, &element_key, &row)
                .unwrap();
        }
    }
    write
        .put(
            Table::Metadata,
            keys::FORMAT_VERSION_KEY,
            &13u32.to_be_bytes(),
        )
        .unwrap();
}

/// Rewrites a database of the current format in the layout of format 11, which kept each
/// relationship record as its own entry under its ID, so a test can open it as an older version.
pub(crate) fn store_as_format_eleven(write: &mut impl WriteTransaction) {
    store_as_format_thirteen(write);
    for (key, value) in write.scan_prefix(Table::RelationshipChunks, &[]).unwrap() {
        let chunk = relationships::parse_key(&key).unwrap();
        for entry in ChunkRef::parse(&value).unwrap().entries() {
            let (slot, record) = entry.unwrap();
            let id = relationships::relationship_at(chunk, slot);
            write
                .put(Table::Relationships, &keys::element_key(id.0), record)
                .unwrap();
        }
        write.delete(Table::RelationshipChunks, &key).unwrap();
    }
    write
        .put(
            Table::Metadata,
            keys::FORMAT_VERSION_KEY,
            &11u32.to_be_bytes(),
        )
        .unwrap();
}

/// Rewrites a database of the current format in the layout of format 10, which kept each
/// relationship record's type and ends at fixed width, so a test can open it as an older version.
pub(crate) fn store_as_format_ten(write: &mut impl WriteTransaction) {
    store_as_format_eleven(write);
    for (key, value) in write.scan_prefix(Table::Relationships, &[]).unwrap() {
        let record = RelationshipRecord::decode(&value).unwrap();
        write
            .put(Table::Relationships, &key, &record.encode_fixed())
            .unwrap();
    }
    write
        .put(
            Table::Metadata,
            keys::FORMAT_VERSION_KEY,
            &10u32.to_be_bytes(),
        )
        .unwrap();
}

/// Rewrites a database of the current format in the layout of format 9, which kept each
/// relationship as one entry under its source in the outgoing table and one under its target in
/// the incoming table, so a test can open it as an older version.
pub(crate) fn store_as_format_nine(write: &mut impl WriteTransaction) {
    store_as_format_ten(write);
    let mut segments: BTreeMap<SegmentId, Vec<adjacency::Stored>> = BTreeMap::new();
    for (key, value) in write.scan_prefix(Table::Adjacency, &[]).unwrap() {
        let (id, _) = adjacency::parse_key(&key).unwrap();
        write.delete(Table::Adjacency, &key).unwrap();
        segments.entry(id).or_default().push((key, value));
    }
    for (id, stored) in segments {
        let table = match id.side {
            Side::Outgoing => Table::Outgoing,
            Side::Incoming => Table::Incoming,
        };
        let edit = SegmentEdit::load(
            &id,
            stored
                .iter()
                .map(|(key, value)| (key.as_slice(), value.as_slice())),
        )
        .unwrap();
        for (position, edges) in edit.slots().unwrap() {
            for edge in edges {
                write
                    .put(
                        table,
                        &keys::adjacency_key(
                            id.node_at(position),
                            Token::new(id.rel_type),
                            edge.relationship,
                        ),
                        &edge.other.0.to_be_bytes(),
                    )
                    .unwrap();
            }
        }
    }
    write
        .put(
            Table::Metadata,
            keys::FORMAT_VERSION_KEY,
            &9u32.to_be_bytes(),
        )
        .unwrap();
}

/// Rewrites a database of the current format in the layout of format 8, which kept each node's
/// properties in one record under its ID, so a test can open it as an older version.
pub(crate) fn store_as_format_eight(write: &mut impl WriteTransaction) {
    store_as_format_nine(write);
    for (node, properties) in take_columns(write) {
        let record = NodeRecord {
            labels: BTreeSet::new(),
            properties,
        }
        .encode();
        write
            .put(Table::Nodes, &keys::element_key(node.0), &record)
            .unwrap();
    }
    write
        .put(
            Table::Metadata,
            keys::FORMAT_VERSION_KEY,
            &8u32.to_be_bytes(),
        )
        .unwrap();
}

/// Rewrites a database of the current format in the layout of format 7, which kept each node's
/// labels at the front of its record and in a label index, so a test can open it as an older
/// version.
pub(crate) fn store_as_format_seven(write: &mut impl WriteTransaction) {
    store_as_format_nine(write);
    let mut sets = BTreeMap::new();
    for (key, value) in write.scan_prefix(Table::LabelSets, &[]).unwrap() {
        sets.insert(
            decode_u32(&key).unwrap(),
            meta::parse_label_set(&value).unwrap(),
        );
        write.delete(Table::LabelSets, &key).unwrap();
        write.delete(Table::LabelSetByTokens, &value).unwrap();
    }
    let mut labels = BTreeMap::new();
    let mut groups = BTreeMap::new();
    for (key, value) in write.scan_prefix(Table::NodeChunks, &[]).unwrap() {
        match meta::parse_key(&key).unwrap().unwrap() {
            (group, meta::Part::Chunk) => {
                let edit = MetaBytes::parse(group, &value).unwrap().to_edit().unwrap();
                groups.insert(group, edit);
            }
            (group, meta::Part::Delta(_)) => {
                let delta = meta::Delta::parse(&key, &value).unwrap();
                let edit: &mut MetaEdit = groups.get_mut(&group).unwrap();
                edit.set(delta.slot, delta.after);
            }
        }
        write.delete(Table::NodeChunks, &key).unwrap();
    }
    for (group, edit) in groups {
        for (slot, set) in edit.live() {
            let held = sets.get(&set.0).cloned().unwrap_or_else(|| Arc::from([]));
            labels.insert(meta::node_at(group, slot), held);
        }
    }
    let mut properties = take_columns(write);
    for (node, held) in labels {
        let mut record = Vec::new();
        crate::value::put_varint(&mut record, held.len());
        for label in held.iter() {
            record.extend_from_slice(&label.id().to_be_bytes());
            write
                .put(Table::LabelIndex, &keys::label_key(*label, node), &[])
                .unwrap();
        }
        let properties = properties.remove(&node).unwrap_or_default();
        record.extend_from_slice(
            &NodeRecord {
                labels: BTreeSet::new(),
                properties,
            }
            .encode(),
        );
        write
            .put(Table::Nodes, &keys::element_key(node.0), &record)
            .unwrap();
    }
    assert!(properties.is_empty(), "a property column holds a dead node");
    write
        .delete(Table::Metadata, keys::NEXT_LABEL_SET_KEY)
        .unwrap();
}

#[derive(Debug, Default)]
struct Gate {
    armed: std::sync::atomic::AtomicBool,
    entered: Mutex<Option<std::sync::mpsc::Sender<()>>>,
    opened: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
}

#[derive(Debug)]
struct GatedStorage {
    inner: RedbStorage,
    gate: Arc<Gate>,
}

struct GatedWrite {
    inner: crate::storage::redb_backend::RedbWrite,
    gate: Arc<Gate>,
}

impl Storage for GatedStorage {
    type Read<'a> = crate::storage::redb_backend::RedbRead;
    type Write<'a> = GatedWrite;

    fn begin_read(&self) -> Result<Self::Read<'_>, StorageError> {
        self.inner.begin_read()
    }

    fn begin_write(&self) -> Result<GatedWrite, StorageError> {
        Ok(GatedWrite {
            inner: self.inner.begin_write()?,
            gate: Arc::clone(&self.gate),
        })
    }

    fn write_copy(snapshot: &Self::Read<'_>, path: &std::path::Path) -> Result<(), StorageError> {
        RedbStorage::write_copy(snapshot, path)
    }
}

impl ReadTransaction for GatedWrite {
    fn get(&self, table: Table, key: &[u8]) -> Result<Option<Vec<u8>>, StorageError> {
        self.inner.get(table, key)
    }

    fn scan_prefix(
        &self,
        table: Table,
        prefix: &[u8],
    ) -> Result<Vec<crate::storage::Entry>, StorageError> {
        self.inner.scan_prefix(table, prefix)
    }

    fn scan_range(
        &self,
        table: Table,
        start: &[u8],
        end: &[u8],
    ) -> Result<Vec<crate::storage::Entry>, StorageError> {
        self.inner.scan_range(table, start, end)
    }

    fn get_with<R, E: From<StorageError>>(
        &self,
        table: Table,
        key: &[u8],
        read: impl FnOnce(&[u8]) -> Result<R, E>,
    ) -> Result<Option<R>, E> {
        self.inner.get_with(table, key, read)
    }

    fn visit_prefix<E: From<StorageError>>(
        &self,
        table: Table,
        prefix: &[u8],
        visit: impl FnMut(&[u8], &[u8]) -> Result<(), E>,
    ) -> Result<(), E> {
        self.inner.visit_prefix(table, prefix, visit)
    }

    fn visit_range<E: From<StorageError>>(
        &self,
        table: Table,
        start: &[u8],
        end: &[u8],
        visit: impl FnMut(&[u8], &[u8]) -> Result<(), E>,
    ) -> Result<(), E> {
        self.inner.visit_range(table, start, end, visit)
    }
}

impl WriteTransaction for GatedWrite {
    fn put(&mut self, table: Table, key: &[u8], value: &[u8]) -> Result<(), StorageError> {
        self.inner.put(table, key, value)
    }

    fn delete(&mut self, table: Table, key: &[u8]) -> Result<(), StorageError> {
        self.inner.delete(table, key)
    }

    fn delete_range(&mut self, table: Table, start: &[u8], end: &[u8]) -> Result<(), StorageError> {
        self.inner.delete_range(table, start, end)
    }

    fn commit(self) -> Result<(), StorageError> {
        if self
            .gate
            .armed
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            if let Some(entered) = lock(&self.gate.entered).take() {
                entered.send(()).unwrap();
            }
            if let Some(opened) = lock(&self.gate.opened).take() {
                opened.recv().unwrap();
            }
        }
        self.inner.commit()
    }

    fn rollback(self) -> Result<(), StorageError> {
        self.inner.rollback()
    }
}

#[test]
fn a_read_begins_while_a_commit_that_creates_an_index_is_writing() {
    let gate = Arc::new(Gate::default());
    let graph = Graph::open(GatedStorage {
        inner: RedbStorage::in_memory().unwrap(),
        gate: Arc::clone(&gate),
    })
    .unwrap();
    let (enter, entered) = std::sync::mpsc::channel();
    let (open, opened) = std::sync::mpsc::channel();
    *lock(&gate.entered) = Some(enter);
    *lock(&gate.opened) = Some(opened);
    gate.armed.store(true, std::sync::atomic::Ordering::SeqCst);
    std::thread::scope(|scope| {
        let writer = scope.spawn(|| {
            let mut write = graph.write().unwrap();
            write
                .create_keyword_index("words", IndexTarget::Label("Doc"), &["text"])
                .unwrap();
            write.commit().unwrap();
        });
        entered.recv().unwrap();
        let (begun, began) = std::sync::mpsc::channel();
        let shared = &graph;
        let reader = scope.spawn(move || {
            let read = shared.read().unwrap();
            begun
                .send((
                    read.indexes().unwrap().len(),
                    read.index_definitions().unwrap().len(),
                ))
                .unwrap();
        });
        let seen = began.recv_timeout(std::time::Duration::from_secs(5));
        open.send(()).unwrap();
        writer.join().unwrap();
        reader.join().unwrap();
        assert_eq!(seen, Ok((0, 0)), "a read waited for the commit");
    });
    let read = graph.read().unwrap();
    assert_eq!(read.indexes().unwrap().len(), 1);
    assert_eq!(read.index_definitions().unwrap().len(), 1);
}
