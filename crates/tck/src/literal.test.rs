use super::*;

fn parse(text: &str) -> Value {
    parse_value(text).unwrap_or_else(|error| panic!("{error}"))
}

fn node(labels: &[&str], properties: &[(&str, Value)]) -> Node {
    Node {
        labels: labels.iter().map(|label| label.to_string()).collect(),
        properties: properties
            .iter()
            .map(|(key, value)| (key.to_string(), value.clone()))
            .collect(),
    }
}

fn rel(rel_type: &str) -> Relationship {
    Relationship {
        rel_type: rel_type.into(),
        properties: Properties::new(),
    }
}

#[test]
fn parses_null_and_booleans() {
    assert_eq!(parse("null"), Value::Null);
    assert_eq!(parse("true"), Value::Boolean(true));
    assert_eq!(parse("false"), Value::Boolean(false));
}

#[test]
fn parses_integers_including_the_extremes() {
    assert_eq!(parse("0"), Value::Integer(0));
    assert_eq!(parse("-42"), Value::Integer(-42));
    assert_eq!(parse("9223372036854775807"), Value::Integer(i64::MAX));
    assert_eq!(parse("-9223372036854775808"), Value::Integer(i64::MIN));
}

#[test]
fn rejects_an_integer_outside_the_i64_range() {
    assert!(parse_value("9223372036854775808").is_err());
}

#[test]
fn parses_floats_in_decimal_and_scientific_form() {
    assert_eq!(parse("1.5"), Value::Float(1.5));
    assert_eq!(parse("-0.000001"), Value::Float(-0.000001));
    assert_eq!(parse(".5"), Value::Float(0.5));
    assert_eq!(parse("1e3"), Value::Float(1000.0));
    assert_eq!(parse("1.23456789e308"), Value::Float(1.23456789e308));
    assert_eq!(
        parse("-1.2635418652381264e305"),
        Value::Float(-1.2635418652381264e305)
    );
    assert_eq!(parse("2.5E-2"), Value::Float(0.025));
}

#[test]
fn parses_negative_zero_as_a_float_with_its_sign() {
    match parse("-0.0") {
        Value::Float(value) => assert!(value == 0.0 && value.is_sign_negative()),
        other => panic!("expected a float, got {other:?}"),
    }
}

#[test]
fn parses_the_special_floats() {
    assert_eq!(parse("NaN"), Value::Float(f64::NAN));
    assert_eq!(parse("Infinity"), Value::Float(f64::INFINITY));
    assert_eq!(parse("Inf"), Value::Float(f64::INFINITY));
    assert_eq!(parse("-Inf"), Value::Float(f64::NEG_INFINITY));
    assert_eq!(parse("-Infinity"), Value::Float(f64::NEG_INFINITY));
}

#[test]
fn rejects_a_decimal_point_without_digits() {
    assert!(parse_value("1.").is_err());
    assert!(parse_value("-").is_err());
}

#[test]
fn parses_strings_with_escaped_quotes_and_backslashes() {
    assert_eq!(parse("'abc'"), Value::String("abc".into()));
    assert_eq!(parse("''"), Value::String(String::new()));
    assert_eq!(parse(r"'it\'s'"), Value::String("it's".into()));
    assert_eq!(parse(r"'\''"), Value::String("'".into()));
    assert_eq!(parse(r"'a\\b'"), Value::String(r"a\b".into()));
    assert_eq!(
        parse("'  spaced, [1]  '"),
        Value::String("  spaced, [1]  ".into())
    );
    assert_eq!(
        parse("'caf\u{e9} \u{1F600}'"),
        Value::String("caf\u{e9} \u{1F600}".into())
    );
}

#[test]
fn rejects_an_unterminated_string_and_a_stray_backslash() {
    assert!(parse_value("'abc").is_err());
    assert!(parse_value(r"'a\nb'").is_err());
}

#[test]
fn parses_lists_including_nested_and_empty() {
    assert_eq!(parse("[]"), Value::List(vec![]));
    assert_eq!(
        parse("[1, 'a', null]"),
        Value::List(vec![
            Value::Integer(1),
            Value::String("a".into()),
            Value::Null
        ])
    );
    assert_eq!(
        parse("[[1], []]"),
        Value::List(vec![
            Value::List(vec![Value::Integer(1)]),
            Value::List(vec![])
        ])
    );
}

#[test]
fn parses_maps_with_null_values() {
    assert_eq!(parse("{}"), Value::Map(Properties::new()));
    assert_eq!(
        parse("{a: null, b: {c: 1}}"),
        Value::Map(
            [
                ("a".to_string(), Value::Null),
                (
                    "b".to_string(),
                    Value::Map([("c".to_string(), Value::Integer(1))].into())
                )
            ]
            .into()
        )
    );
}

#[test]
fn parses_nodes_with_labels_and_properties() {
    assert_eq!(parse("()"), Value::Node(node(&[], &[])));
    assert_eq!(parse("(:A)"), Value::Node(node(&["A"], &[])));
    assert_eq!(
        parse("(:A:B {name: 'b'})"),
        Value::Node(node(&["A", "B"], &[("name", Value::String("b".into()))]))
    );
    assert_eq!(
        parse("({name: 'c'})"),
        Value::Node(node(&[], &[("name", Value::String("c".into()))]))
    );
}

#[test]
fn parses_relationships_with_and_without_properties() {
    assert_eq!(parse("[:T]"), Value::Relationship(rel("T")));
    assert_eq!(
        parse("[:T {k: 1}]"),
        Value::Relationship(Relationship {
            rel_type: "T".into(),
            properties: [("k".to_string(), Value::Integer(1))].into(),
        })
    );
}

#[test]
fn tells_a_relationship_from_a_list_of_values() {
    assert_eq!(parse("[1]"), Value::List(vec![Value::Integer(1)]));
    assert_eq!(parse("[:T]"), Value::Relationship(rel("T")));
}

#[test]
fn parses_a_zero_length_path() {
    assert_eq!(
        parse("<()>"),
        Value::Path(Path {
            start: Node::default(),
            steps: vec![]
        })
    );
}

#[test]
fn parses_paths_in_both_directions() {
    assert_eq!(
        parse("<(:A)-[:T]->(:B)<-[:U]-(:C)>"),
        Value::Path(Path {
            start: node(&["A"], &[]),
            steps: vec![
                PathStep {
                    relationship: rel("T"),
                    direction: Direction::Forward,
                    node: node(&["B"], &[]),
                },
                PathStep {
                    relationship: rel("U"),
                    direction: Direction::Backward,
                    node: node(&["C"], &[]),
                },
            ],
        })
    );
}

#[test]
fn ignores_whitespace_between_tokens() {
    assert_eq!(parse("  [ 1 ,2 ]  "), parse("[1,2]"));
    assert_eq!(parse("{ a : 1 }"), parse("{a: 1}"));
}

#[test]
fn reports_where_a_malformed_value_goes_wrong() {
    for malformed in [
        "[1, 2",
        "(:A",
        "{a 1}",
        "foo",
        "1 2",
        "",
        "<(:A)-[:T]-(:B)>",
    ] {
        assert!(
            parse_value(malformed).is_err(),
            "{malformed} should not parse"
        );
    }
    let error = parse_value("[1, ?]").expect_err("malformed list");
    assert_eq!(error.position, 4);
}
