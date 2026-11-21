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
    let id = write.create_node(&record).unwrap();
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
    let a = write.create_node(&node_with(&[])).unwrap();
    let b = write.create_node(&node_with(&[])).unwrap();
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
    let a = write.create_node(&node_with(&[])).unwrap();
    let b = write.create_node(&node_with(&[])).unwrap();
    let c = write.create_node(&node_with(&[])).unwrap();
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
    let a = write.create_node(&node_with(&[])).unwrap();
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
    let a = write.create_node(&node_with(&[person])).unwrap();
    let b = write.create_node(&node_with(&[])).unwrap();
    let c = write.create_node(&node_with(&[person, admin])).unwrap();
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
fn deleting_a_node_with_relationships_is_refused_until_they_are_gone() {
    let graph = memory_graph();
    let mut write = graph.write().unwrap();
    let knows: Token<RelationshipType> = write.intern_token("KNOWS").unwrap();
    let a = write.create_node(&node_with(&[])).unwrap();
    let b = write.create_node(&node_with(&[])).unwrap();
    let edge = write
        .create_relationship(&relationship(knows, a, b))
        .unwrap();

    assert!(matches!(
        write.delete_node(a),
        Err(GraphError::NodeHasRelationships(_))
    ));
    assert!(matches!(
        write.delete_node(b),
        Err(GraphError::NodeHasRelationships(_))
    ));
    assert!(write.node(a).unwrap().is_some());

    write.delete_relationship(edge).unwrap();
    assert_eq!(write.relationship(edge).unwrap(), None);
    assert!(write.adjacent(a, Direction::Both, None).unwrap().is_empty());
    assert!(write.adjacent(b, Direction::Both, None).unwrap().is_empty());
    write.delete_node(a).unwrap();
    write.delete_node(b).unwrap();
    assert_eq!(write.node_ids().unwrap(), Vec::new());
    assert!(matches!(
        write.delete_node(a),
        Err(GraphError::NodeNotFound(_))
    ));
    assert!(matches!(
        write.delete_relationship(edge),
        Err(GraphError::RelationshipNotFound(_))
    ));
}

#[test]
fn properties_can_be_set_replaced_and_removed_on_nodes_and_relationships() {
    let graph = memory_graph();
    let mut write = graph.write().unwrap();
    let knows: Token<RelationshipType> = write.intern_token("KNOWS").unwrap();
    let since: Token<PropertyKey> = write.intern_token("since").unwrap();
    let a = write.create_node(&node_with(&[])).unwrap();
    let b = write.create_node(&node_with(&[])).unwrap();
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
        write.create_node(&node_with(&[])).unwrap();
    }

    let mut write = graph.write().unwrap();
    assert_eq!(write.node_ids().unwrap(), Vec::new());
    assert_eq!(write.create_node(&node_with(&[])).unwrap(), NodeId(0));
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
        let a = write.create_node(&node_with(&[person])).unwrap();
        let b = write.create_node(&node_with(&[person])).unwrap();
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
        let a = write.create_node(&node_with(&[])).unwrap();
        let b = write.create_node(&node_with(&[])).unwrap();
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
    let fresh = write.create_node(&node_with(&[])).unwrap();
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
