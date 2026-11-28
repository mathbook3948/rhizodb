use super::records::Properties;
use super::*;
use crate::storage::test_engine;
use crate::storage::test_engine::TestStorage;

thread_local! {
    static READS: std::cell::Cell<Reads> = const { std::cell::Cell::new(Reads::NONE) };
}

/// A read whose count on one thread a test checks.
#[derive(Clone, Copy)]
pub(crate) enum Read {
    TokenLookup,
    RelationshipDecode,
    MetaLoad,
}

/// How many reads of each [`Read`] a thread made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Reads {
    /// Tokens looked up in the tokens every transaction shares.
    pub(crate) token_lookups: usize,
    /// Relationship records decoded whole.
    pub(crate) relationship_decodes: usize,
    /// Node meta chunks loaded from storage.
    pub(crate) meta_loads: usize,
}

impl Reads {
    const NONE: Self = Self {
        token_lookups: 0,
        relationship_decodes: 0,
        meta_loads: 0,
    };
}

pub(crate) fn note(read: Read) {
    READS.with(|reads| {
        let mut counts = reads.get();
        match read {
            Read::TokenLookup => counts.token_lookups += 1,
            Read::RelationshipDecode => counts.relationship_decodes += 1,
            Read::MetaLoad => counts.meta_loads += 1,
        }
        reads.set(counts);
    });
}

/// What `run` returns, with the reads it made on this thread.
pub(crate) fn counted<R>(run: impl FnOnce() -> R) -> (R, Reads) {
    let before = READS.with(std::cell::Cell::get);
    let result = run();
    let after = READS.with(std::cell::Cell::get);
    let reads = Reads {
        token_lookups: after.token_lookups - before.token_lookups,
        relationship_decodes: after.relationship_decodes - before.relationship_decodes,
        meta_loads: after.meta_loads - before.meta_loads,
    };
    (result, reads)
}

fn memory_graph() -> Graph<TestStorage> {
    Graph::open(test_engine::in_memory()).unwrap()
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
    assert_eq!(&*read.token_name(other).unwrap(), "other");
    assert_eq!(&*read.token_name(as_key).unwrap(), "name");
    assert_eq!(read.lookup_token::<Label>("other").unwrap(), Some(other));
    assert_eq!(
        read.lookup_token::<RelationshipType>("other").unwrap(),
        None
    );
}

#[test]
fn a_token_id_a_rolled_back_transaction_used_names_the_token_committed_with_it_later() {
    let graph = memory_graph();
    let mut write = graph.write().unwrap();
    let kept: Token<Label> = write.intern_token("Kept").unwrap();
    write.commit().unwrap();
    let read = graph.read().unwrap();
    assert_eq!(&*read.token_name(kept).unwrap(), "Kept");

    let mut ghost = graph.write().unwrap();
    let ghost_token: Token<Label> = ghost.intern_token("Ghost").unwrap();
    assert_eq!(&*ghost.token_name(ghost_token).unwrap(), "Ghost");
    assert_eq!(&*ghost.token_name(kept).unwrap(), "Kept");
    assert_eq!(read.lookup_token::<Label>("Ghost").unwrap(), None);
    ghost.rollback().unwrap();

    let mut real = graph.write().unwrap();
    let real_token: Token<Label> = real.intern_token("Real").unwrap();
    assert_eq!(real_token, ghost_token);
    real.commit().unwrap();
    assert_eq!(read.lookup_token::<Label>("Ghost").unwrap(), None);
    let later = graph.read().unwrap();
    assert_eq!(&*later.token_name(real_token).unwrap(), "Real");
    assert_eq!(later.lookup_token::<Label>("Ghost").unwrap(), None);
    let write = graph.write().unwrap();
    assert_eq!(&*write.token_name(real_token).unwrap(), "Real");
    assert_eq!(write.lookup_token::<Label>("Ghost").unwrap(), None);
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
    let path = directory.path().join("graph.rhizodb");
    let (person, a, b, edge) = {
        let graph = Graph::open(test_engine::open(&path)).unwrap();
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

    let graph = Graph::open(test_engine::open(&path)).unwrap();
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
    let path = directory.path().join("graph.rhizodb");
    let (last_node, last_edge) = {
        let graph = Graph::open(test_engine::open(&path)).unwrap();
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

    let graph = Graph::open(test_engine::open(&path)).unwrap();
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
    let storage = test_engine::in_memory();
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
    let path = directory.path().join("graph.rhizodb");
    Graph::open(test_engine::open(&path)).unwrap();

    let storage = test_engine::open(&path);
    let stored = storage
        .begin_read()
        .unwrap()
        .get(Table::Metadata, keys::FORMAT_VERSION_KEY)
        .unwrap();
    assert_eq!(stored, Some(FORMAT_VERSION.to_be_bytes().to_vec()));
    assert!(Graph::open(storage).is_ok());
}

#[test]
fn the_oldest_format_version_opens_and_is_upgraded_to_the_current_version() {
    let storage = test_engine::in_memory();
    let mut write = storage.begin_write().unwrap();
    write
        .put(
            Table::Metadata,
            keys::FORMAT_VERSION_KEY,
            &OLDEST_FORMAT_VERSION.to_be_bytes(),
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
fn a_format_version_older_than_the_oldest_is_rejected_on_open() {
    let storage = test_engine::in_memory();
    let mut write = storage.begin_write().unwrap();
    let older = OLDEST_FORMAT_VERSION - 1;
    write
        .put(
            Table::Metadata,
            keys::FORMAT_VERSION_KEY,
            &older.to_be_bytes(),
        )
        .unwrap();
    write.commit().unwrap();

    assert!(matches!(
        Graph::open(storage),
        Err(GraphError::UnsupportedFormatVersion(found)) if found == older
    ));
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
    let storage = test_engine::in_memory();
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

fn many_relationships(graph: &Graph<TestStorage>) -> BTreeMap<RelationshipId, RelationshipRecord> {
    let mut expected = BTreeMap::new();
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
    assert!(expected.keys().last().unwrap().0 >= 1 << 16);
    expected
}

fn assert_holds_relationships(
    graph: &Graph<TestStorage>,
    expected: &BTreeMap<RelationshipId, RelationshipRecord>,
) {
    let read = graph.read().unwrap();
    for (id, record) in expected {
        assert_eq!(read.relationship(*id).unwrap().as_ref(), Some(record));
    }
    let mut count = 0;
    read.visit_relationship_records(|_, _| {
        count += 1;
        Ok(())
    })
    .unwrap();
    assert_eq!(count, expected.len());
    assert_eq!(read.check_integrity().unwrap(), vec![]);
    graph.check_relationship_chunks().unwrap();
}

#[test]
fn a_database_of_format_eleven_moves_every_relationship_record_into_its_chunk() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph.rhizodb");
    let expected = {
        let graph = Graph::open(test_engine::open(&path)).unwrap();
        let expected = many_relationships(&graph);
        let mut previous = graph.storage.begin_write().unwrap();
        store_as_format_eleven(&mut previous);
        previous.commit().unwrap();
        expected
    };
    for _ in 0..2 {
        let graph = Graph::open(test_engine::open(&path)).unwrap();
        assert_holds_relationships(&graph, &expected);
        assert_eq!(
            graph
                .read()
                .unwrap()
                .transaction
                .scan_prefix(Table::Relationships, &[])
                .unwrap(),
            vec![]
        );
    }
}

/// Rewrites a database of the current format in the layout of format `older`, 14 or 15.
pub(crate) fn store_as_format(older: u32, write: &mut impl WriteTransaction) {
    match older {
        14 => store_as_format_fourteen(write),
        15 => store_as_format_fifteen(write),
        _ => unreachable!("no test stores format {older}"),
    }
}

fn assert_upgrades_every_relationship_chunk(older: u32) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph.rhizodb");
    let expected = {
        let graph = Graph::open(test_engine::open(&path)).unwrap();
        let expected = many_relationships(&graph);
        let mut previous = graph.storage.begin_write().unwrap();
        store_as_format(older, &mut previous);
        previous.commit().unwrap();
        expected
    };
    let older = test_engine::open(&path)
        .begin_read()
        .unwrap()
        .scan_prefix(Table::RelationshipChunks, &[])
        .unwrap();
    assert!(older.len() > 2048);
    assert!(
        older
            .iter()
            .all(|(key, value)| ChunkRef::parse(key, value).is_err())
    );
    for _ in 0..2 {
        let graph = Graph::open(test_engine::open(&path)).unwrap();
        assert_holds_relationships(&graph, &expected);
        let upgraded = graph
            .read()
            .unwrap()
            .transaction
            .scan_prefix(Table::RelationshipChunks, &[])
            .unwrap();
        assert_eq!(upgraded.len(), older.len());
        for ((key, value), (older_key, older_value)) in upgraded.iter().zip(&older) {
            assert_eq!(key, older_key);
            assert!(ChunkRef::parse(key, value).is_ok());
            assert_eq!(
                relationships::stored_records(key, value),
                relationships::stored_records(older_key, older_value)
            );
        }
    }
}

#[test]
fn a_database_of_format_fourteen_seals_every_relationship_chunk_with_its_checksum() {
    assert_upgrades_every_relationship_chunk(14);
}

#[test]
fn a_database_of_format_fifteen_stores_every_relationship_chunk_in_columns() {
    assert_upgrades_every_relationship_chunk(15);
}

#[test]
fn an_upgrade_from_format_fifteen_takes_fewer_bytes_for_the_same_relationships() {
    let graph = memory_graph();
    many_relationships(&graph);
    let bytes = |graph: &Graph<TestStorage>| -> usize {
        graph
            .storage
            .begin_read()
            .unwrap()
            .scan_prefix(Table::RelationshipChunks, &[])
            .unwrap()
            .iter()
            .map(|(_, value)| value.len())
            .sum()
    };
    let columns = bytes(&graph);
    let mut previous = graph.storage.begin_write().unwrap();
    store_as_format_fifteen(&mut previous);
    previous.commit().unwrap();
    let records = bytes(&graph);
    assert!(
        columns * 4 < records * 3,
        "{columns} bytes in columns, {records} in records"
    );
}

#[test]
fn an_upgrade_from_format_fifteen_refuses_a_damaged_chunk() {
    let unframed = |records: &[(usize, Vec<u8>)], key: &[u8]| {
        let mut value = relationships::in_records_layout(key, records);
        value.push(0);
        relationships::with_checksum(key, value)
    };
    let undecodable = |_: &[(usize, Vec<u8>)], key: &[u8]| {
        relationships::in_records_layout(key, &[(0, vec![0xff; 3])])
    };
    let unsealed = |records: &[(usize, Vec<u8>)], key: &[u8]| {
        let mut value = relationships::in_records_layout(key, records);
        value[1] ^= 1;
        value
    };
    for damage in [
        unframed as fn(&[(usize, Vec<u8>)], &[u8]) -> Vec<u8>,
        undecodable,
        unsealed,
    ] {
        let graph = memory_graph();
        let mut write = graph.write().unwrap();
        let knows: Token<RelationshipType> = write.intern_token("KNOWS").unwrap();
        let node = write.create_node(node_with(&[])).unwrap();
        write
            .create_relationship(&relationship(knows, node, node))
            .unwrap();
        write.commit().unwrap();
        let mut previous = graph.storage.begin_write().unwrap();
        store_as_format_fifteen(&mut previous);
        let (key, value) = previous
            .scan_prefix(Table::RelationshipChunks, &[])
            .unwrap()
            .remove(0);
        let records = relationships::stored_records(&key, &value);
        previous
            .put(Table::RelationshipChunks, &key, &damage(&records, &key))
            .unwrap();
        previous.commit().unwrap();
        let Graph { storage, .. } = graph;

        assert!(matches!(
            Graph::open(storage),
            Err(GraphError::Decode(_) | GraphError::Corrupt(_))
        ));
    }
}

#[test]
fn an_upgrade_from_format_fourteen_refuses_to_seal_a_damaged_chunk() {
    let unframed = |value: &[u8], _: &[u8]| {
        let mut value = value.to_vec();
        value.push(0);
        value
    };
    let undecodable = |_: &[u8], key: &[u8]| {
        let record = [0xff; 3];
        assert!(RelationshipRecord::decode(&record).is_err());
        relationships::unsealed(key, &[(0, record.to_vec())])
    };
    for damage in [unframed as fn(&[u8], &[u8]) -> Vec<u8>, undecodable] {
        let graph = memory_graph();
        let mut write = graph.write().unwrap();
        let knows: Token<RelationshipType> = write.intern_token("KNOWS").unwrap();
        let node = write.create_node(node_with(&[])).unwrap();
        write
            .create_relationship(&relationship(knows, node, node))
            .unwrap();
        write.commit().unwrap();
        let mut previous = graph.storage.begin_write().unwrap();
        store_as_format_fourteen(&mut previous);
        let (key, value) = previous
            .scan_prefix(Table::RelationshipChunks, &[])
            .unwrap()
            .remove(0);
        previous
            .put(Table::RelationshipChunks, &key, &damage(&value, &key))
            .unwrap();
        previous.commit().unwrap();
        let Graph { storage, .. } = graph;

        assert!(matches!(
            Graph::open(storage),
            Err(GraphError::Decode(_) | GraphError::Corrupt(_))
        ));
    }
}

/// Rewrites a database of the current format in the layout of format 15, whose relationship
/// chunks held each record after its length, so a test can open it as an older version.
pub(crate) fn store_as_format_fifteen(write: &mut impl WriteTransaction) {
    for (key, value) in write.scan_prefix(Table::RelationshipChunks, &[]).unwrap() {
        let records = relationships::stored_records(&key, &value);
        write
            .put(
                Table::RelationshipChunks,
                &key,
                &relationships::in_records_layout(&key, &records),
            )
            .unwrap();
    }
    write
        .put(
            Table::Metadata,
            keys::FORMAT_VERSION_KEY,
            &15u32.to_be_bytes(),
        )
        .unwrap();
}

/// Rewrites a database of the current format in the layout of format 14, whose relationship
/// chunks carried no checksum, so a test can open it as an older version.
pub(crate) fn store_as_format_fourteen(write: &mut impl WriteTransaction) {
    for (key, value) in write.scan_prefix(Table::RelationshipChunks, &[]).unwrap() {
        let records = relationships::stored_records(&key, &value);
        write
            .put(
                Table::RelationshipChunks,
                &key,
                &relationships::unsealed(&key, &records),
            )
            .unwrap();
    }
    write
        .put(
            Table::Metadata,
            keys::FORMAT_VERSION_KEY,
            &14u32.to_be_bytes(),
        )
        .unwrap();
}

/// Rewrites a database of the current format in the layout of format 13, which kept each
/// element's posting of a term as its own entry under the element's ID, so a test can open it as
/// an older version.
pub(crate) fn store_as_format_thirteen(write: &mut impl WriteTransaction) {
    store_as_format_fourteen(write);
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
    for (key, value) in write.scan_prefix(Table::RelationshipChunks, &[]).unwrap() {
        let chunk = relationships::parse_key(&key).unwrap();
        for entry in ChunkRef::parse(&key, &value).unwrap().entries() {
            let (slot, record) = entry.unwrap();
            let id = relationships::relationship_at(chunk, slot);
            write
                .put(Table::Relationships, &keys::element_key(id.0), &record)
                .unwrap();
        }
        write.delete(Table::RelationshipChunks, &key).unwrap();
    }
    store_as_format_thirteen(write);
    write
        .put(
            Table::Metadata,
            keys::FORMAT_VERSION_KEY,
            &11u32.to_be_bytes(),
        )
        .unwrap();
}

#[derive(Debug, Default)]
struct Gate {
    node_chunk_reads: std::sync::atomic::AtomicUsize,
    armed: std::sync::atomic::AtomicBool,
    entered: Mutex<Option<std::sync::mpsc::Sender<()>>>,
    opened: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
}

#[derive(Debug)]
struct GatedStorage {
    inner: TestStorage,
    gate: Arc<Gate>,
}

struct GatedWrite<'a> {
    inner: <TestStorage as Storage>::Write<'a>,
    gate: Arc<Gate>,
}

impl Storage for GatedStorage {
    type Read<'a> = <TestStorage as Storage>::Read<'a>;
    type Write<'a> = GatedWrite<'a>;

    fn begin_read(&self) -> Result<Self::Read<'_>, StorageError> {
        self.inner.begin_read()
    }

    fn begin_write(&self) -> Result<GatedWrite<'_>, StorageError> {
        Ok(GatedWrite {
            inner: self.inner.begin_write()?,
            gate: Arc::clone(&self.gate),
        })
    }

    fn write_copy(snapshot: &Self::Read<'_>, path: &std::path::Path) -> Result<(), StorageError> {
        TestStorage::write_copy(snapshot, path)
    }
}

impl GatedWrite<'_> {
    fn count(&self, table: Table) {
        if table == Table::NodeChunks {
            self.gate
                .node_chunk_reads
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

impl ReadTransaction for GatedWrite<'_> {
    fn get(&self, table: Table, key: &[u8]) -> Result<Option<Vec<u8>>, StorageError> {
        self.count(table);
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
        self.count(table);
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

impl WriteTransaction for GatedWrite<'_> {
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
        inner: test_engine::in_memory(),
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

#[test]
fn creating_relationships_between_committed_nodes_reads_their_node_meta_once() {
    let gate = Arc::new(Gate::default());
    let graph = Graph::open(GatedStorage {
        inner: test_engine::in_memory(),
        gate: Arc::clone(&gate),
    })
    .unwrap();
    let mut write = graph.write().unwrap();
    let source = write.create_node(NodeRecord::default()).unwrap();
    let target = write.create_node(NodeRecord::default()).unwrap();
    let rel_type = write.intern_token("R").unwrap();
    write.commit().unwrap();
    let mut write = graph.write().unwrap();
    gate.node_chunk_reads
        .store(0, std::sync::atomic::Ordering::SeqCst);
    for _ in 0..100 {
        write
            .create_relationship(&RelationshipRecord {
                rel_type,
                source,
                target,
                properties: Properties::new(),
            })
            .unwrap();
    }
    let reads = gate
        .node_chunk_reads
        .load(std::sync::atomic::Ordering::SeqCst);
    assert!(reads <= 4, "{reads} node meta reads");
    write.commit().unwrap();
    assert_eq!(graph.read().unwrap().check_integrity().unwrap(), vec![]);
}

#[test]
fn labels_that_change_nothing_on_a_committed_node_read_its_node_meta_once() {
    let gate = Arc::new(Gate::default());
    let graph = Graph::open(GatedStorage {
        inner: test_engine::in_memory(),
        gate: Arc::clone(&gate),
    })
    .unwrap();
    let mut write = graph.write().unwrap();
    let label = write.intern_token("A").unwrap();
    let other = write.intern_token("B").unwrap();
    let node = write
        .create_node(NodeRecord {
            labels: BTreeSet::from([label]),
            properties: Properties::new(),
        })
        .unwrap();
    write.commit().unwrap();
    let mut write = graph.write().unwrap();
    gate.node_chunk_reads
        .store(0, std::sync::atomic::Ordering::SeqCst);
    for _ in 0..100 {
        write.add_labels(node, &[label]).unwrap();
        write.remove_labels(node, &[other]).unwrap();
    }
    let reads = gate
        .node_chunk_reads
        .load(std::sync::atomic::Ordering::SeqCst);
    assert!(reads <= 4, "{reads} node meta reads");
    write.commit().unwrap();
    assert_eq!(graph.read().unwrap().check_integrity().unwrap(), vec![]);
}

#[test]
fn opening_a_database_of_an_older_format_as_its_writer_checkpoints_the_upgrade() {
    let storage = test_engine::in_memory();
    let mut write = storage.begin_write().unwrap();
    write
        .put(
            Table::Metadata,
            keys::FORMAT_VERSION_KEY,
            &OLDEST_FORMAT_VERSION.to_be_bytes(),
        )
        .unwrap();
    write.commit().unwrap();
    let before = storage.header();
    let graph = Graph::open(storage).unwrap();
    let after = graph.storage.header();
    assert!(after.generation > before.generation);
    assert!(after.checkpoint_lsn > before.checkpoint_lsn);
    let checkpointed = graph.storage.export(&[]).unwrap();
    let stored = GraphTx::bare(graph.storage.begin_read().unwrap())
        .transaction
        .get(Table::Metadata, keys::FORMAT_VERSION_KEY)
        .unwrap();
    assert_eq!(stored, Some(FORMAT_VERSION.to_be_bytes().to_vec()));
    assert_eq!(checkpointed.position(), after.checkpoint_lsn);
}

#[test]
fn opening_a_new_or_current_database_checkpoints_nothing() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph.rhizodb");
    let storage = test_engine::open(&path);
    let before = storage.header();
    let graph = Graph::open(storage).unwrap();
    assert_eq!(graph.storage.header().generation, before.generation);
    drop(graph);
    let storage = test_engine::open(&path);
    let before = storage.header();
    let graph = Graph::open(storage).unwrap();
    assert_eq!(graph.storage.header().generation, before.generation);
}

#[test]
fn a_read_only_graph_opens_only_the_current_format_and_tokenizer() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph.rhizodb");
    drop(Graph::open(test_engine::open(&path)).unwrap());
    let read_only = || {
        let storage = crate::engine::any_fs::open_path_read_only(
            &path,
            crate::engine::store::EngineOptions::default(),
        )
        .unwrap();
        Graph::open_as(storage, Access::ReadOnly)
    };
    assert!(read_only().is_ok());
    let rewrite = |key: &[u8], value: &[u8]| {
        let storage = test_engine::open(&path);
        let mut write = storage.begin_write().unwrap();
        write.put(Table::Metadata, key, value).unwrap();
        write.commit().unwrap();
    };
    rewrite(
        keys::FORMAT_VERSION_KEY,
        &(FORMAT_VERSION - 1).to_be_bytes(),
    );
    assert!(matches!(read_only(), Err(GraphError::ReadOnly(_))));
    rewrite(
        keys::FORMAT_VERSION_KEY,
        &(FORMAT_VERSION | UPGRADING).to_be_bytes(),
    );
    assert!(matches!(read_only(), Err(GraphError::ReadOnly(_))));
    rewrite(keys::FORMAT_VERSION_KEY, &99u32.to_be_bytes());
    assert!(matches!(
        read_only(),
        Err(GraphError::UnsupportedFormatVersion(99))
    ));
    rewrite(keys::FORMAT_VERSION_KEY, &FORMAT_VERSION.to_be_bytes());
    assert!(read_only().is_ok());
    rewrite(keys::TOKENIZER_KEY, b"another tokenizer");
    assert!(matches!(read_only(), Err(GraphError::ReadOnly(_))));
}

#[test]
fn an_upgrade_an_older_build_began_runs_the_steps_that_build_lacked_before_resuming() {
    for (from, step) in [
        (14, UpgradeStep::Tokenizer),
        (13, UpgradeStep::Tokenizer),
        (13, UpgradeStep::PostingBlocks),
    ] {
        let graph = memory_graph();
        let mut write = graph.write().unwrap();
        let knows: Token<RelationshipType> = write.intern_token("KNOWS").unwrap();
        let node = write.create_node(node_with(&[])).unwrap();
        let mut record = relationship(knows, node, node);
        let key: Token<PropertyKey> = write.intern_token("k").unwrap();
        record.properties.insert(key, PropertyValue::Integer(7));
        let id = write.create_relationship(&record).unwrap();
        write.commit().unwrap();
        let mut previous = graph.storage.begin_write().unwrap();
        store_as_format_fourteen(&mut previous);
        previous
            .put(
                Table::Metadata,
                keys::FORMAT_VERSION_KEY,
                &(from | UPGRADING).to_be_bytes(),
            )
            .unwrap();
        let mut progress = 14u32.to_be_bytes().to_vec();
        progress.push(step.code());
        previous
            .put(Table::Metadata, keys::UPGRADE_PROGRESS_KEY, &progress)
            .unwrap();
        previous
            .put(Table::Metadata, keys::TOKENIZER_KEY, b"another tokenizer")
            .unwrap();
        previous.commit().unwrap();
        let Graph { storage, .. } = graph;

        let graph = Graph::open(storage).unwrap();
        let read = graph.read().unwrap();
        assert_eq!(
            read.relationship(id).unwrap().as_ref(),
            Some(&record),
            "from {from}, resumed at {step:?}"
        );
        assert_eq!(read.check_integrity().unwrap(), vec![]);
        graph.check_relationship_chunks().unwrap();
    }
}

fn keyword_database(path: &std::path::Path, ghost: bool) {
    let db = crate::Database::open(path).unwrap();
    db.create_keyword_index("text", IndexTarget::Label("Doc"), &["text"])
        .unwrap();
    let run = |query: &str| {
        db.execute(query, &crate::Parameters::new()).unwrap();
    };
    run(
        "UNWIND range(0, 49) AS i CREATE (:Doc {i: i, text: 'river ' + toString(i)})-[:R {k: i}]->(:Doc {i: i + 100, text: 'ocean'})",
    );
    if ghost {
        run("CREATE (:Doc {i: 999, text: 'ghost'})");
    }
}

fn keyword_index_id(write: &impl WriteTransaction) -> u32 {
    let (key, _) = write
        .scan_prefix(Table::KeywordTotals, &[])
        .unwrap()
        .remove(0);
    u32::from_be_bytes(key[..4].try_into().unwrap())
}

fn index_entries(write: &impl WriteTransaction, id: u32) -> Vec<(Table, Vec<u8>, Vec<u8>)> {
    [
        Table::KeywordPostings,
        Table::KeywordDocuments,
        Table::KeywordTotals,
    ]
    .into_iter()
    .flat_map(|table| {
        write
            .scan_prefix(table, &keys::index_prefix(id))
            .unwrap()
            .into_iter()
            .map(move |(key, value)| (table, key, value))
    })
    .collect()
}

fn mark_upgrading(write: &mut impl WriteTransaction, from: u32, step: UpgradeStep, cursor: &[u8]) {
    write
        .put(
            Table::Metadata,
            keys::FORMAT_VERSION_KEY,
            &(from | UPGRADING).to_be_bytes(),
        )
        .unwrap();
    let mut progress = 14u32.to_be_bytes().to_vec();
    progress.push(step.code());
    if !cursor.is_empty() {
        progress.push(1);
        progress.extend_from_slice(cursor);
    }
    write
        .put(Table::Metadata, keys::UPGRADE_PROGRESS_KEY, &progress)
        .unwrap();
}

fn assert_upgraded_whole(path: &std::path::Path, context: &str) {
    let db = crate::Database::open(path).unwrap();
    let count = |text: &str| {
        db.execute(
            &format!(
                "CALL db.index.keyword.queryNodes('text', '{text}', 1000) YIELD node RETURN count(*)"
            ),
            &crate::Parameters::new(),
        )
        .unwrap()
        .rows()
        .to_vec()
    };
    let integer = |value: i64| vec![vec![crate::Value::Integer(value)]];
    assert_eq!(count("river"), integer(50), "{context}");
    assert_eq!(count("ghost"), integer(0), "{context}");
    assert_eq!(
        db.execute(
            "MATCH ()-[r:R]->() RETURN sum(r.k)",
            &crate::Parameters::new()
        )
        .unwrap()
        .rows(),
        integer(1225),
        "{context}"
    );
    db.check().unwrap();
}

#[test]
fn a_resumed_rebuild_of_an_older_build_rebuilds_every_keyword_index_whole() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("filling.db");
    keyword_database(&path, false);
    {
        let storage = test_engine::open(&path);
        let mut write = storage.begin_write().unwrap();
        let id = keyword_index_id(&write);
        for (table, key, _) in index_entries(&write, id) {
            write.delete(table, &key).unwrap();
        }
        store_as_format_fourteen(&mut write);
        let mut filling = id.to_be_bytes().to_vec();
        filling.push(1);
        filling.extend_from_slice(&(1u64 << 40).to_be_bytes());
        mark_upgrading(&mut write, 14, UpgradeStep::Tokenizer, &filling);
        write
            .put(Table::Metadata, keys::TOKENIZER_KEY, b"another tokenizer")
            .unwrap();
        write.commit().unwrap();
    }
    assert_upgraded_whole(&path, "an older build's cursor past every element");

    let path = directory.path().join("clearing.db");
    keyword_database(&path, true);
    let (id, stale) = {
        let storage = test_engine::open(&path);
        let write = storage.begin_write().unwrap();
        let id = keyword_index_id(&write);
        (id, index_entries(&write, id))
    };
    crate::Database::open(&path)
        .unwrap()
        .execute("MATCH (d:Doc {i: 999}) DELETE d", &crate::Parameters::new())
        .unwrap();
    {
        let storage = test_engine::open(&path);
        let mut write = storage.begin_write().unwrap();
        for (table, key, value) in stale {
            write.put(table, &key, &value).unwrap();
        }
        store_as_format_fourteen(&mut write);
        let mut clearing = id.to_be_bytes().to_vec();
        clearing.extend_from_slice(&[2, 3]);
        clearing.extend_from_slice(&keys::index_prefix(id));
        clearing.push(0x42);
        mark_upgrading(&mut write, 14, UpgradeStep::Tokenizer, &clearing);
        write
            .put(Table::Metadata, keys::TOKENIZER_KEY, b"another tokenizer")
            .unwrap();
        write.commit().unwrap();
    }
    assert_upgraded_whole(&path, "an older build's cursor past the posting tables");
}

#[test]
fn a_resumed_upgrade_of_an_older_build_keeps_the_steps_that_build_had_not_finished() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("postings.db");
    keyword_database(&path, false);
    {
        let storage = test_engine::open(&path);
        let mut write = storage.begin_write().unwrap();
        store_as_format_thirteen(&mut write);
        mark_upgrading(&mut write, 13, UpgradeStep::PostingBlocks, &[]);
        write.commit().unwrap();
    }
    assert_upgraded_whole(&path, "an older build's posting blocks step");
}

#[test]
fn an_upgrade_a_newer_build_began_is_refused_as_a_newer_format() {
    let storage = test_engine::in_memory();
    let mut write = storage.begin_write().unwrap();
    write
        .put(
            Table::Metadata,
            keys::FORMAT_VERSION_KEY,
            &((FORMAT_VERSION - 1) | UPGRADING).to_be_bytes(),
        )
        .unwrap();
    let mut progress = (FORMAT_VERSION + 1).to_be_bytes().to_vec();
    progress.push(UpgradeStep::PostingBlocks.code());
    write
        .put(Table::Metadata, keys::UPGRADE_PROGRESS_KEY, &progress)
        .unwrap();
    write.commit().unwrap();
    assert!(matches!(
        Graph::open(storage),
        Err(GraphError::UnsupportedFormatVersion(version)) if version == FORMAT_VERSION + 1
    ));
}

#[test]
fn opening_a_database_of_another_tokenizer_as_its_writer_checkpoints_the_rebuilt_indexes() {
    let storage = test_engine::in_memory();
    let mut write = storage.begin_write().unwrap();
    write
        .put(
            Table::Metadata,
            keys::FORMAT_VERSION_KEY,
            &FORMAT_VERSION.to_be_bytes(),
        )
        .unwrap();
    write
        .put(Table::Metadata, keys::TOKENIZER_KEY, b"another tokenizer")
        .unwrap();
    write.commit().unwrap();
    let before = storage.header();
    let graph = Graph::open(storage).unwrap();
    let after = graph.storage.header();
    assert!(after.generation > before.generation);
    assert!(after.checkpoint_lsn > before.checkpoint_lsn);
    let stored = GraphTx::bare(graph.storage.begin_read().unwrap())
        .transaction
        .get(Table::Metadata, keys::TOKENIZER_KEY)
        .unwrap();
    assert_eq!(stored, Some(tokenize::fingerprint()));
}

#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "written only at commit")]
fn writing_node_columns_before_commit_fails_a_debug_build() {
    let graph = memory_graph();
    let mut write = graph.write().unwrap();
    let id = write.create_node(node_with(&[])).unwrap();
    let key = write.intern_token::<PropertyKey>("a").unwrap();
    write
        .set_node_property(id, key, PropertyValue::Integer(1))
        .unwrap();
    _ = write.flush_columns();
}
