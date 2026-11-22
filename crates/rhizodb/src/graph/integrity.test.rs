use super::super::records::Properties;
use super::*;
use crate::graph::Graph;
use crate::graph::Token;
use crate::storage::Storage;
use crate::storage::WriteTransaction;
use crate::storage::redb_backend::RedbStorage;
use crate::value::PropertyValue;

const PERSON: u32 = 0;
const KNOWS: u32 = 0;
const NAME: u32 = 0;
const ALICE: NodeId = NodeId(0);
const BOB: NodeId = NodeId(1);
const CAROL: NodeId = NodeId(2);
const FRIENDSHIP: RelationshipId = RelationshipId(0);
const LOOP: RelationshipId = RelationshipId(1);

type Write<'a> = <RedbStorage as Storage>::Write<'a>;

fn healthy_graph() -> Graph<RedbStorage> {
    let graph = Graph::open(RedbStorage::in_memory().unwrap()).unwrap();
    let mut write = graph.write().unwrap();
    let person = write.intern_token::<Label>("Person").unwrap();
    let knows = write.intern_token::<RelationshipType>("KNOWS").unwrap();
    let name = write.intern_token::<PropertyKey>("name").unwrap();
    let mut alice = NodeRecord::default();
    alice.labels.insert(person);
    alice
        .properties
        .insert(name, PropertyValue::String("Alice".to_owned()));
    assert_eq!(write.create_node(&alice).unwrap(), ALICE);
    let mut bob = NodeRecord::default();
    bob.labels.insert(person);
    assert_eq!(write.create_node(&bob).unwrap(), BOB);
    assert_eq!(write.create_node(&NodeRecord::default()).unwrap(), CAROL);
    let friendship = RelationshipRecord {
        rel_type: knows,
        source: ALICE,
        target: BOB,
        properties: Properties::new(),
    };
    assert_eq!(write.create_relationship(&friendship).unwrap(), FRIENDSHIP);
    let self_loop = RelationshipRecord {
        source: CAROL,
        target: CAROL,
        ..friendship.clone()
    };
    assert_eq!(write.create_relationship(&self_loop).unwrap(), LOOP);
    write.commit().unwrap();
    graph
}

fn violations_of(graph: &Graph<RedbStorage>) -> Vec<IntegrityViolation> {
    graph.read().unwrap().check_integrity().unwrap()
}

fn corrupt(graph: &Graph<RedbStorage>, edit: impl FnOnce(&mut Write<'_>)) {
    let mut write = graph.storage.begin_write().unwrap();
    edit(&mut write);
    write.commit().unwrap();
}

fn assert_caught(edit: impl FnOnce(&mut Write<'_>), expected: IntegrityViolation) {
    let graph = healthy_graph();
    assert_eq!(violations_of(&graph), vec![], "graph must start healthy");
    corrupt(&graph, edit);
    let violations = violations_of(&graph);
    assert!(
        violations.contains(&expected),
        "expected {expected:?} in {violations:?}"
    );
}

fn adjacency_key(node: NodeId, relationship: RelationshipId) -> [u8; 20] {
    keys::adjacency_key(node, Token::new(KNOWS), relationship)
}

fn remove_token(write: &mut Write<'_>, tag: u8, id: u32, name: &str) {
    write
        .delete(Table::TokenById, &keys::token_id_key(tag, id))
        .unwrap();
    write
        .delete(Table::TokenByName, &keys::token_name_key(tag, name))
        .unwrap();
}

#[test]
fn a_graph_built_through_the_api_passes() {
    assert_eq!(violations_of(&healthy_graph()), vec![]);
}

#[test]
fn a_graph_after_edits_and_deletes_passes() {
    let graph = healthy_graph();
    let mut write = graph.write().unwrap();
    let person = Token::<Label>::new(PERSON);
    let scratch = write.create_node(&NodeRecord::default()).unwrap();
    write.add_label(scratch, person).unwrap();
    write.delete_relationship(LOOP).unwrap();
    write.remove_label(BOB, person).unwrap();
    write.add_label(CAROL, person).unwrap();
    write
        .set_node_property(ALICE, Token::new(NAME), PropertyValue::Integer(1))
        .unwrap();
    write.remove_label(scratch, person).unwrap();
    write.delete_node(scratch).unwrap();
    write.commit().unwrap();
    assert_eq!(violations_of(&graph), vec![]);
}

#[test]
fn an_empty_database_passes() {
    let graph = Graph::open(RedbStorage::in_memory().unwrap()).unwrap();
    assert_eq!(violations_of(&graph), vec![]);
}

#[test]
fn a_missing_format_version_is_caught() {
    assert_caught(
        |write| {
            write
                .delete(Table::Metadata, keys::FORMAT_VERSION_KEY)
                .unwrap();
        },
        IntegrityViolation::FormatVersionMissing,
    );
}

#[test]
fn a_foreign_format_version_is_caught() {
    assert_caught(
        |write| {
            write
                .put(
                    Table::Metadata,
                    keys::FORMAT_VERSION_KEY,
                    &9u32.to_be_bytes(),
                )
                .unwrap();
        },
        IntegrityViolation::FormatVersionMismatch(9),
    );
}

#[test]
fn a_relationship_with_a_missing_source_is_caught() {
    assert_caught(
        |write| {
            write
                .delete(Table::Nodes, &keys::element_key(ALICE.0))
                .unwrap();
        },
        IntegrityViolation::DanglingEndpoint {
            relationship: FRIENDSHIP,
            node: ALICE,
        },
    );
}

#[test]
fn a_relationship_with_a_missing_target_is_caught() {
    assert_caught(
        |write| {
            write
                .delete(Table::Nodes, &keys::element_key(BOB.0))
                .unwrap();
        },
        IntegrityViolation::DanglingEndpoint {
            relationship: FRIENDSHIP,
            node: BOB,
        },
    );
}

#[test]
fn a_relationship_missing_its_outgoing_entry_is_caught() {
    assert_caught(
        |write| {
            write
                .delete(Table::Outgoing, &adjacency_key(ALICE, FRIENDSHIP))
                .unwrap();
        },
        IntegrityViolation::MissingAdjacency {
            table: Table::Outgoing,
            relationship: FRIENDSHIP,
        },
    );
}

#[test]
fn a_relationship_missing_its_incoming_entry_is_caught() {
    assert_caught(
        |write| {
            write
                .delete(Table::Incoming, &adjacency_key(BOB, FRIENDSHIP))
                .unwrap();
        },
        IntegrityViolation::MissingAdjacency {
            table: Table::Incoming,
            relationship: FRIENDSHIP,
        },
    );
}

#[test]
fn an_outgoing_entry_without_a_relationship_record_is_caught() {
    let ghost = RelationshipId(7);
    assert_caught(
        |write| {
            write
                .put(
                    Table::Outgoing,
                    &adjacency_key(ALICE, ghost),
                    &BOB.0.to_be_bytes(),
                )
                .unwrap();
        },
        IntegrityViolation::OrphanAdjacency {
            table: Table::Outgoing,
            node: ALICE,
            relationship: ghost,
        },
    );
}

#[test]
fn an_incoming_entry_without_a_relationship_record_is_caught() {
    let ghost = RelationshipId(7);
    assert_caught(
        |write| {
            write
                .put(
                    Table::Incoming,
                    &adjacency_key(BOB, ghost),
                    &ALICE.0.to_be_bytes(),
                )
                .unwrap();
        },
        IntegrityViolation::OrphanAdjacency {
            table: Table::Incoming,
            node: BOB,
            relationship: ghost,
        },
    );
}

#[test]
fn an_adjacency_entry_with_the_wrong_far_node_is_caught() {
    assert_caught(
        |write| {
            write
                .put(
                    Table::Outgoing,
                    &adjacency_key(ALICE, FRIENDSHIP),
                    &CAROL.0.to_be_bytes(),
                )
                .unwrap();
        },
        IntegrityViolation::OrphanAdjacency {
            table: Table::Outgoing,
            node: ALICE,
            relationship: FRIENDSHIP,
        },
    );
}

#[test]
fn an_adjacency_entry_under_the_wrong_type_is_caught() {
    let other_type = Token::<RelationshipType>::new(5);
    assert_caught(
        |write| {
            write
                .put(
                    Table::Incoming,
                    &keys::adjacency_key(BOB, other_type, FRIENDSHIP),
                    &ALICE.0.to_be_bytes(),
                )
                .unwrap();
        },
        IntegrityViolation::OrphanAdjacency {
            table: Table::Incoming,
            node: BOB,
            relationship: FRIENDSHIP,
        },
    );
}

#[test]
fn a_node_label_missing_from_the_label_index_is_caught() {
    assert_caught(
        |write| {
            write
                .delete(
                    Table::LabelIndex,
                    &keys::label_key(Token::new(PERSON), ALICE),
                )
                .unwrap();
        },
        IntegrityViolation::MissingLabelIndexEntry {
            label: PERSON,
            node: ALICE,
        },
    );
}

#[test]
fn a_label_index_entry_the_node_does_not_carry_is_caught() {
    assert_caught(
        |write| {
            write
                .put(
                    Table::LabelIndex,
                    &keys::label_key(Token::new(PERSON), CAROL),
                    &[],
                )
                .unwrap();
        },
        IntegrityViolation::OrphanLabelIndexEntry {
            label: PERSON,
            node: CAROL,
        },
    );
}

#[test]
fn a_label_index_entry_for_a_missing_node_is_caught() {
    assert_caught(
        |write| {
            write
                .put(
                    Table::LabelIndex,
                    &keys::label_key(Token::new(PERSON), NodeId(40)),
                    &[],
                )
                .unwrap();
        },
        IntegrityViolation::OrphanLabelIndexEntry {
            label: PERSON,
            node: NodeId(40),
        },
    );
}

#[test]
fn a_token_missing_from_the_id_table_is_caught() {
    assert_caught(
        |write| {
            write
                .delete(Table::TokenById, &keys::token_id_key(Label::TAG, PERSON))
                .unwrap();
        },
        IntegrityViolation::TokenTablesDisagree {
            tag: Label::TAG,
            id: PERSON,
        },
    );
}

#[test]
fn a_token_missing_from_the_name_table_is_caught() {
    assert_caught(
        |write| {
            write
                .delete(
                    Table::TokenByName,
                    &keys::token_name_key(RelationshipType::TAG, "KNOWS"),
                )
                .unwrap();
        },
        IntegrityViolation::TokenTablesDisagree {
            tag: RelationshipType::TAG,
            id: KNOWS,
        },
    );
}

#[test]
fn token_tables_that_name_one_id_differently_are_caught() {
    assert_caught(
        |write| {
            write
                .put(
                    Table::TokenById,
                    &keys::token_id_key(PropertyKey::TAG, NAME),
                    b"nom",
                )
                .unwrap();
        },
        IntegrityViolation::TokenTablesDisagree {
            tag: PropertyKey::TAG,
            id: NAME,
        },
    );
}

#[test]
fn a_property_key_used_by_a_record_but_unnamed_is_caught() {
    assert_caught(
        |write| remove_token(write, PropertyKey::TAG, NAME, "name"),
        IntegrityViolation::UnnamedToken {
            tag: PropertyKey::TAG,
            id: NAME,
        },
    );
}

#[test]
fn a_label_used_by_a_node_but_unnamed_is_caught() {
    assert_caught(
        |write| remove_token(write, Label::TAG, PERSON, "Person"),
        IntegrityViolation::UnnamedToken {
            tag: Label::TAG,
            id: PERSON,
        },
    );
}

#[test]
fn a_relationship_type_used_by_a_relationship_but_unnamed_is_caught() {
    assert_caught(
        |write| remove_token(write, RelationshipType::TAG, KNOWS, "KNOWS"),
        IntegrityViolation::UnnamedToken {
            tag: RelationshipType::TAG,
            id: KNOWS,
        },
    );
}

#[test]
fn a_token_of_an_unknown_kind_is_caught() {
    assert_caught(
        |write| {
            write
                .put(
                    Table::TokenByName,
                    &keys::token_name_key(9, "x"),
                    &0u32.to_be_bytes(),
                )
                .unwrap();
            write
                .put(Table::TokenById, &keys::token_id_key(9, 0), b"x")
                .unwrap();
        },
        IntegrityViolation::UnknownTokenTag(9),
    );
}

#[test]
fn a_node_counter_that_is_not_above_the_highest_node_is_caught() {
    assert_caught(
        |write| {
            write
                .put(
                    Table::Metadata,
                    keys::NEXT_NODE_ID_KEY,
                    &CAROL.0.to_be_bytes(),
                )
                .unwrap();
        },
        IntegrityViolation::CounterNotAboveAllocated {
            counter: Counter::NodeId,
            highest: CAROL.0,
        },
    );
}

#[test]
fn a_missing_node_counter_is_caught() {
    assert_caught(
        |write| {
            write
                .delete(Table::Metadata, keys::NEXT_NODE_ID_KEY)
                .unwrap();
        },
        IntegrityViolation::CounterNotAboveAllocated {
            counter: Counter::NodeId,
            highest: CAROL.0,
        },
    );
}

#[test]
fn a_relationship_counter_that_is_not_above_the_highest_relationship_is_caught() {
    assert_caught(
        |write| {
            write
                .put(
                    Table::Metadata,
                    keys::NEXT_RELATIONSHIP_ID_KEY,
                    &LOOP.0.to_be_bytes(),
                )
                .unwrap();
        },
        IntegrityViolation::CounterNotAboveAllocated {
            counter: Counter::RelationshipId,
            highest: LOOP.0,
        },
    );
}

#[test]
fn a_token_counter_that_is_not_above_the_highest_token_is_caught() {
    assert_caught(
        |write| {
            write
                .put(
                    Table::Metadata,
                    &keys::next_token_key(Label::TAG),
                    &u64::from(PERSON).to_be_bytes(),
                )
                .unwrap();
        },
        IntegrityViolation::CounterNotAboveAllocated {
            counter: Counter::Token(Label::TAG),
            highest: u64::from(PERSON),
        },
    );
}

#[test]
fn an_unreadable_node_record_is_caught() {
    assert_caught(
        |write| {
            write
                .put(Table::Nodes, &keys::element_key(BOB.0), &[0xff])
                .unwrap();
        },
        IntegrityViolation::Undecodable {
            table: Table::Nodes,
            key: keys::element_key(BOB.0).to_vec(),
        },
    );
}

#[test]
fn an_unreadable_relationship_record_is_caught() {
    assert_caught(
        |write| {
            write
                .put(Table::Relationships, &keys::element_key(LOOP.0), &[1])
                .unwrap();
        },
        IntegrityViolation::Undecodable {
            table: Table::Relationships,
            key: keys::element_key(LOOP.0).to_vec(),
        },
    );
}

#[test]
fn every_violation_is_reported_not_just_the_first() {
    let graph = healthy_graph();
    corrupt(&graph, |write| {
        write
            .delete(Table::Metadata, keys::FORMAT_VERSION_KEY)
            .unwrap();
        write
            .delete(Table::Outgoing, &adjacency_key(ALICE, FRIENDSHIP))
            .unwrap();
    });
    let violations = violations_of(&graph);
    assert!(violations.contains(&IntegrityViolation::FormatVersionMissing));
    assert!(violations.contains(&IntegrityViolation::MissingAdjacency {
        table: Table::Outgoing,
        relationship: FRIENDSHIP,
    }));
}

fn run_query(graph: &Graph<RedbStorage>, query: &str) -> Result<(), crate::error::Error> {
    crate::query::execute(graph, query, &crate::result::Parameters::new()).map(|_| ())
}

fn count(graph: &Graph<RedbStorage>, query: &str) -> usize {
    crate::query::execute(graph, query, &crate::result::Parameters::new())
        .unwrap()
        .rows()
        .len()
}

#[test]
fn a_graph_stays_consistent_after_queries_that_delete_connected_nodes() {
    let graph = Graph::open(RedbStorage::in_memory().unwrap()).unwrap();
    run_query(&graph, "CREATE (:A)-[:R]->(:B)").unwrap();

    let refused = run_query(&graph, "MATCH (a:A)-[r]->() DELETE a").unwrap_err();
    assert_eq!(refused.kind(), crate::error::ErrorKind::DeleteConnectedNode);
    assert_eq!(count(&graph, "MATCH (n) RETURN n"), 2);
    assert_eq!(count(&graph, "MATCH ()-[r]->() RETURN r"), 1);
    assert_eq!(violations_of(&graph), vec![]);

    run_query(&graph, "MATCH (a:A)-[r]->() DELETE a DELETE r").unwrap();
    assert_eq!(count(&graph, "MATCH (n:B) RETURN n"), 1);
    assert_eq!(count(&graph, "MATCH (n) RETURN n"), 1);
    assert_eq!(count(&graph, "MATCH ()-[r]->() RETURN r"), 0);
    assert_eq!(violations_of(&graph), vec![]);
}

#[test]
fn clearing_a_property_that_never_existed_does_not_intern_its_name() {
    let graph = Graph::open(RedbStorage::in_memory().unwrap()).unwrap();
    run_query(&graph, "CREATE (:A {k: 1})-[:R]->(:B)").unwrap();
    for query in [
        "MATCH (n) REMOVE n.typo",
        "MATCH (n) SET n.typo = null",
        "MATCH ()-[r]->() REMOVE r.typo",
        "MATCH ()-[r]->() SET r.typo = null",
    ] {
        run_query(&graph, query).unwrap();
        let known = graph
            .read()
            .unwrap()
            .lookup_token::<PropertyKey>("typo")
            .unwrap();
        assert_eq!(known, None, "{query}");
    }
    assert_eq!(violations_of(&graph), vec![]);

    run_query(&graph, "MATCH (n:A) REMOVE n.k").unwrap();
    assert_eq!(count(&graph, "MATCH (n:A) WHERE n.k IS NULL RETURN n"), 1);
    run_query(&graph, "MATCH (n:A) SET n.k = 2").unwrap();
    assert_eq!(count(&graph, "MATCH (n:A) WHERE n.k = 2 RETURN n"), 1);
    assert_eq!(violations_of(&graph), vec![]);
}
