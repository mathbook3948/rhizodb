use super::*;

fn prop(entity: Entity, key: &str, value: i64) -> PropertyEntry {
    PropertyEntry {
        entity,
        key: key.into(),
        value: Value::Integer(value),
    }
}

fn snapshot(nodes: &[u64], labels: &[&str], properties: Vec<PropertyEntry>) -> GraphSnapshot {
    GraphSnapshot {
        nodes: nodes.to_vec(),
        relationships: vec![],
        labels: labels.iter().map(|label| label.to_string()).collect(),
        properties,
    }
}

#[test]
fn an_unchanged_graph_has_no_side_effects() {
    let graph = snapshot(&[1, 2], &["A"], vec![prop(Entity::Node(1), "k", 1)]);

    assert_eq!(graph.diff(&graph.clone()), SideEffects::default());
}

#[test]
fn counts_added_and_removed_nodes_separately() {
    let before = snapshot(&[1, 2], &[], vec![]);
    let after = snapshot(&[2, 3, 4], &[], vec![]);

    let effects = before.diff(&after);

    assert_eq!(effects.added.nodes, 2);
    assert_eq!(effects.removed.nodes, 1);
}

#[test]
fn counts_relationships_by_id() {
    let before = GraphSnapshot::default();
    let after = GraphSnapshot {
        relationships: vec![7],
        ..GraphSnapshot::default()
    };

    assert_eq!(before.diff(&after).added.relationships, 1);
    assert_eq!(after.diff(&before).removed.relationships, 1);
}

#[test]
fn counts_distinct_labels_not_labelled_nodes() {
    let before = snapshot(&[1], &["A"], vec![]);
    let after = snapshot(&[1, 2, 3], &["A", "B"], vec![]);

    let effects = before.diff(&after);

    assert_eq!(effects.added.labels, 1);
    assert_eq!(effects.removed.labels, 0);
}

#[test]
fn changing_a_property_value_is_one_removal_and_one_addition() {
    let before = snapshot(&[1], &[], vec![prop(Entity::Node(1), "k", 1)]);
    let after = snapshot(&[1], &[], vec![prop(Entity::Node(1), "k", 2)]);

    let effects = before.diff(&after);

    assert_eq!(effects.added.properties, 1);
    assert_eq!(effects.removed.properties, 1);
}

#[test]
fn moving_a_property_to_another_entity_is_one_removal_and_one_addition() {
    let before = snapshot(&[1, 2], &[], vec![prop(Entity::Node(1), "k", 1)]);
    let after = snapshot(&[1, 2], &[], vec![prop(Entity::Node(2), "k", 1)]);

    let effects = before.diff(&after);

    assert_eq!(effects.added.properties, 1);
    assert_eq!(effects.removed.properties, 1);
}

#[test]
fn a_node_and_a_relationship_with_the_same_id_do_not_share_properties() {
    let before = snapshot(&[1], &[], vec![prop(Entity::Node(1), "k", 1)]);
    let after = snapshot(&[1], &[], vec![prop(Entity::Relationship(1), "k", 1)]);

    assert_eq!(before.diff(&after).added.properties, 1);
}

#[test]
fn identical_property_triples_are_counted_as_a_multiset() {
    let one = prop(Entity::Node(1), "k", 1);
    let before = snapshot(&[1], &[], vec![one.clone()]);
    let after = snapshot(&[1], &[], vec![one.clone(), one]);

    assert_eq!(before.diff(&after).added.properties, 1);
    assert_eq!(after.diff(&before).removed.properties, 1);
}
