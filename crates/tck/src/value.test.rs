use super::*;

fn int_list(items: &[i64]) -> Value {
    Value::List(items.iter().copied().map(Value::Integer).collect())
}

fn node(labels: &[&str], properties: &[(&str, Value)]) -> Value {
    Value::Node(Node {
        labels: labels.iter().map(|label| label.to_string()).collect(),
        properties: properties
            .iter()
            .map(|(key, value)| (key.to_string(), value.clone()))
            .collect(),
    })
}

#[test]
fn nodes_are_equal_by_labels_and_properties() {
    let a = node(&["A", "B"], &[("name", Value::String("x".into()))]);
    let same = node(&["B", "A"], &[("name", Value::String("x".into()))]);

    assert_eq!(a, same);
    assert_ne!(a, node(&["A"], &[("name", Value::String("x".into()))]));
    assert_ne!(a, node(&["A", "B"], &[("name", Value::String("y".into()))]));
    assert_ne!(a, node(&["A", "B"], &[]));
}

#[test]
fn relationships_are_equal_by_type_and_properties() {
    let rel = |rel_type: &str, k: i64| {
        Value::Relationship(Relationship {
            rel_type: rel_type.into(),
            properties: [("k".to_string(), Value::Integer(k))].into(),
        })
    };

    assert_eq!(rel("T", 1), rel("T", 1));
    assert_ne!(rel("T", 1), rel("U", 1));
    assert_ne!(rel("T", 1), rel("T", 2));
}

#[test]
fn paths_distinguish_direction() {
    let hop = |direction| {
        Value::Path(Path {
            start: Node::default(),
            steps: vec![PathStep {
                relationship: Relationship {
                    rel_type: "T".into(),
                    properties: Properties::new(),
                },
                direction,
                node: Node::default(),
            }],
        })
    };

    assert_eq!(hop(Direction::Forward), hop(Direction::Forward));
    assert_ne!(hop(Direction::Forward), hop(Direction::Backward));
}

#[test]
fn integers_and_floats_are_different_values() {
    assert_ne!(Value::Integer(1), Value::Float(1.0));
    assert_eq!(Value::Float(1.5), Value::Float(1.5));
}

#[test]
fn nan_equals_nan_and_negative_zero_equals_zero() {
    assert_eq!(Value::Float(f64::NAN), Value::Float(f64::NAN));
    assert_eq!(Value::Float(-0.0), Value::Float(0.0));
    assert_ne!(Value::Float(f64::NAN), Value::Float(0.0));
}

#[test]
fn null_inside_a_map_is_not_the_same_as_a_missing_key() {
    let with_null = Value::Map([("a".to_string(), Value::Null)].into());

    assert_ne!(with_null, Value::Map(Properties::new()));
    assert_eq!(with_null.clone(), with_null);
}

#[test]
fn strict_lists_care_about_order() {
    assert_ne!(int_list(&[1, 2]), int_list(&[2, 1]));
    assert_eq!(int_list(&[1, 2]), int_list(&[1, 2]));
}

#[test]
fn ignoring_element_order_still_counts_duplicates() {
    let mode = Lists::IgnoreElementOrder;

    assert!(int_list(&[1, 2, 3]).equals(&int_list(&[3, 1, 2]), mode));
    assert!(!int_list(&[1, 1, 2]).equals(&int_list(&[1, 2, 2]), mode));
    assert!(!int_list(&[1, 2]).equals(&int_list(&[1, 2, 2]), mode));
}

#[test]
fn ignoring_element_order_applies_to_nested_lists_and_maps() {
    let mode = Lists::IgnoreElementOrder;
    let wrap = |inner: Value| Value::Map([("xs".to_string(), Value::List(vec![inner]))].into());

    assert!(wrap(int_list(&[1, 2])).equals(&wrap(int_list(&[2, 1])), mode));
    assert!(!wrap(int_list(&[1, 2])).equals(&wrap(int_list(&[2, 1])), Lists::Strict));
}

#[test]
fn ignoring_element_order_does_not_match_different_elements() {
    let mode = Lists::IgnoreElementOrder;

    assert!(!int_list(&[1, 2]).equals(&int_list(&[1, 3]), mode));
    assert!(
        !Value::List(vec![Value::Integer(1)]).equals(&Value::List(vec![Value::Float(1.0)]), mode)
    );
}
