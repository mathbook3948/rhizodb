use std::collections::BTreeMap;

use proptest::prelude::*;

use super::*;
use crate::database::Database;
use crate::test_scale;
use crate::value::tests::property_value;
use crate::value::tests::same_bits;

fn round_trip(value: &PropertyValue) -> (String, PropertyValue) {
    let mut text = String::new();
    write_value(&mut text, value);
    let parsed = json::parse(&text).unwrap_or_else(|error| panic!("{text}: {error}"));
    let read = read_value(parsed, 0).unwrap_or_else(|error| panic!("{text}: {error}"));
    (text, read)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(test_scale::cases(512)))]

    #[test]
    fn every_value_reads_back_bit_exactly_and_writes_the_same_text(value in property_value()) {
        let (text, read) = round_trip(&value);
        prop_assert!(same_bits(&value, &read), "{text}");
        let mut again = String::new();
        write_value(&mut again, &read);
        prop_assert_eq!(text, again);
    }

    #[test]
    fn any_line_parses_or_fails_without_panicking(bytes in prop::collection::vec(any::<u8>(), 0..64)) {
        let text = String::from_utf8_lossy(&bytes);
        if let Ok(parsed) = json::parse(&text) {
            let _ = read_value(parsed, 0);
        }
    }

    #[test]
    fn any_string_reads_back(text in any::<String>()) {
        let mut written = String::new();
        json::write_string(&mut written, &text);
        prop_assert_eq!(json::parse(&written), Ok(Json::String(text)));
    }

    #[test]
    fn every_finite_float_is_written_as_a_float(bits in any::<u64>()) {
        let value = f64::from_bits(bits);
        prop_assume!(value.is_finite());
        let mut written = String::new();
        json::write_finite_float(&mut written, value);
        match json::parse(&written) {
            Ok(Json::Float(read)) => prop_assert_eq!(read.to_bits(), bits),
            other => prop_assert!(false, "{written} parsed as {other:?}"),
        }
    }
}

#[test]
fn the_special_floats_keep_their_bits() {
    for value in [
        -0.0,
        0.0,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NAN,
        -f64::NAN,
        f64::from_bits(0x7ff0_0000_0000_0001),
        f64::MIN_POSITIVE,
        f64::from_bits(1),
        f64::MAX,
        1e16,
        1.0,
    ] {
        let (text, read) = round_trip(&PropertyValue::Float(value));
        assert!(same_bits(&PropertyValue::Float(value), &read), "{text}");
    }
    assert_eq!(round_trip(&PropertyValue::Float(-0.0)).0, "-0.0");
    assert_eq!(round_trip(&PropertyValue::Float(1.0)).0, "1.0");
    assert_eq!(round_trip(&PropertyValue::Integer(1)).0, "1");
    assert_eq!(
        round_trip(&PropertyValue::Float(f64::INFINITY)).0,
        r#"{"$float":"7ff0000000000000"}"#
    );
}

#[test]
fn the_parser_refuses_what_json_does_not_allow() {
    for text in [
        "",
        "01",
        "1.",
        ".5",
        "1e",
        "+1",
        "-",
        "1e400",
        "9223372036854775808",
        "NaN",
        "[1,]",
        "{\"a\":1,}",
        "{a:1}",
        "\"\\x\"",
        "\"\u{1}\"",
        "\"\\ud800\"",
        "\"\\udc00\"",
        "\"abc",
        "[1] 2",
        "tru",
        "nul",
    ] {
        assert!(json::parse(text).is_err(), "{text:?} parsed");
    }
    assert_eq!(
        json::parse("-9223372036854775808"),
        Ok(Json::Integer(i64::MIN))
    );
    assert_eq!(
        json::parse("\"\\ud83d\\ude00\\u00e9\\/\""),
        Ok(Json::String("\u{1f600}\u{e9}/".to_owned()))
    );
    let deep = format!(
        "{}{}",
        "[".repeat(json::MAX_NESTING + 1),
        "]".repeat(json::MAX_NESTING + 1)
    );
    assert!(json::parse(&deep).is_err());
}

#[test]
fn a_value_that_cannot_be_stored_is_refused() {
    for text in [
        "null",
        "[null]",
        "{}",
        r#"{"$date":"2024-02-30"}"#,
        r#"{"$date":"24-01-01"}"#,
        r#"{"$localtime":"24:00:00.000000000"}"#,
        r#"{"$localtime":"12:00:00"}"#,
        r#"{"$time":"12:00:00.000000000+19:00:00"}"#,
        r#"{"$datetime":"2024-01-01T00:00:00.000000000+00:00:00[Nowhere/Land]"}"#,
        r#"{"$float":"3ff0000000000000"}"#,
        r#"{"$float":"7FF0000000000000"}"#,
        r#"{"$bytes":"0"}"#,
        r#"{"$bytes":"AB"}"#,
        r#"{"$duration":{"months":0,"days":0,"seconds":0,"nanoseconds":1000000000}}"#,
        r#"{"$duration":{"months":0,"days":0,"seconds":0}}"#,
        r#"{"$date":"2024-01-01","$time":"x"}"#,
        r#"{"$map":"x"}"#,
    ] {
        let parsed = json::parse(text).unwrap();
        assert!(read_value(parsed, 0).is_err(), "{text} was read");
    }
    let mut nested = String::from("1");
    for _ in 0..MAX_DEPTH {
        nested = format!("[{nested}]");
    }
    assert!(read_value(json::parse(&nested).unwrap(), 0).is_ok());
    let deeper = format!("[{nested}]");
    assert!(read_value(json::parse(&deeper).unwrap(), 0).is_err());
}

#[derive(Clone, Debug)]
enum Op {
    Node(Vec<usize>, Vec<(usize, PropertyValue)>),
    Relationship(usize, usize, usize, Vec<(usize, PropertyValue)>),
    DeleteNode(usize),
    DeleteRelationship(usize),
    SetProperty(usize, usize, PropertyValue),
    RemoveProperty(usize, usize),
    Commit,
}

const LABELS: [&str; 4] = ["A", "B", "", "lab\"el\n\u{e9}"];
const TYPES: [&str; 3] = ["T", "U", "\u{0}type"];
const KEYS: [&str; 5] = ["p", "q", "text", "vec", "k\\ey"];

fn properties() -> impl Strategy<Value = Vec<(usize, PropertyValue)>> {
    let vector = prop::collection::vec(-10.0f64..10.0, 2).prop_map(|items| {
        PropertyValue::List(items.into_iter().map(PropertyValue::Float).collect())
    });
    let text = "[a-c ]{0,12}".prop_map(PropertyValue::String);
    prop::collection::vec(
        (0..KEYS.len(), prop_oneof![property_value(), vector, text]),
        0..4,
    )
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => (prop::collection::vec(0..LABELS.len(), 0..3), properties())
            .prop_map(|(labels, properties)| Op::Node(labels, properties)),
        3 => (any::<usize>(), any::<usize>(), 0..TYPES.len(), properties())
            .prop_map(|(start, end, rel_type, properties)| {
                Op::Relationship(start, end, rel_type, properties)
            }),
        1 => any::<usize>().prop_map(Op::DeleteNode),
        1 => any::<usize>().prop_map(Op::DeleteRelationship),
        2 => (any::<usize>(), 0..KEYS.len(), property_value())
            .prop_map(|(node, key, value)| Op::SetProperty(node, key, value)),
        1 => (any::<usize>(), 0..KEYS.len()).prop_map(|(node, key)| Op::RemoveProperty(node, key)),
        1 => Just(Op::Commit),
    ]
}

fn tokens<T: WriteTransaction>(
    tx: &mut GraphTx<T>,
    entries: &[(usize, PropertyValue)],
) -> Properties {
    entries
        .iter()
        .map(|(key, value)| (tx.intern_token(KEYS[*key]).unwrap(), value.clone()))
        .collect()
}

fn build(db: &Database, ops: &[Op], indexes: u8) {
    if indexes & 1 != 0 {
        db.create_keyword_index("words", IndexTarget::Label("A"), &["text", "p"])
            .unwrap();
    }
    if indexes & 2 != 0 {
        db.create_vector_index(
            "vectors",
            IndexTarget::Label("B"),
            "vec",
            2,
            VectorMetric::DotProduct,
        )
        .unwrap();
    }
    if indexes & 4 != 0 {
        db.create_property_index("by_p", IndexTarget::RelationshipType("T"), "p")
            .unwrap();
    }
    if indexes & 8 != 0 {
        db.create_property_index("by_q", IndexTarget::Label(""), "q")
            .unwrap();
    }
    let mut nodes: Vec<NodeId> = Vec::new();
    let mut relationships: Vec<RelationshipId> = Vec::new();
    let mut tx = db.graph().write().unwrap();
    for op in ops {
        match op {
            Op::Node(labels, entries) => {
                let labels = labels
                    .iter()
                    .map(|label| tx.intern_token(LABELS[*label]).unwrap())
                    .collect();
                let properties = tokens(&mut tx, entries);
                nodes.push(tx.create_node(NodeRecord { labels, properties }).unwrap());
            }
            Op::Relationship(start, end, rel_type, entries) if !nodes.is_empty() => {
                let record = RelationshipRecord {
                    rel_type: tx.intern_token(TYPES[*rel_type]).unwrap(),
                    source: nodes[start % nodes.len()],
                    target: nodes[end % nodes.len()],
                    properties: tokens(&mut tx, entries),
                };
                relationships.push(tx.create_relationship(&record).unwrap());
            }
            Op::DeleteNode(at) if !nodes.is_empty() => {
                let node = nodes.remove(at % nodes.len());
                for adjacent in tx
                    .adjacent(node, crate::graph::Direction::Both, None)
                    .unwrap()
                {
                    tx.delete_relationship(adjacent.relationship).unwrap();
                    relationships.retain(|id| *id != adjacent.relationship);
                }
                tx.delete_node(node).unwrap();
            }
            Op::DeleteRelationship(at) if !relationships.is_empty() => {
                let relationship = relationships.remove(at % relationships.len());
                tx.delete_relationship(relationship).unwrap();
            }
            Op::SetProperty(at, key, value) if !nodes.is_empty() => {
                let key = tx.intern_token(KEYS[*key]).unwrap();
                tx.set_node_property(nodes[at % nodes.len()], key, value.clone())
                    .unwrap();
            }
            Op::RemoveProperty(at, key) if !nodes.is_empty() => {
                let key = tx.intern_token(KEYS[*key]).unwrap();
                tx.remove_node_property(nodes[at % nodes.len()], key)
                    .unwrap();
            }
            Op::Commit => {
                tx.commit().unwrap();
                tx = db.graph().write().unwrap();
            }
            _ => {}
        }
    }
    tx.commit().unwrap();
}

type Elements = Vec<(u64, Vec<String>, BTreeMap<String, PropertyValue>)>;

fn elements(db: &Database) -> (Elements, Elements, (u64, u64)) {
    let tx = db.graph().read().unwrap();
    let named = |properties: Properties| -> BTreeMap<String, PropertyValue> {
        properties
            .into_iter()
            .map(|(key, value)| (tx.token_name(key).unwrap(), value))
            .collect()
    };
    let mut nodes = Vec::new();
    tx.visit_nodes(|id, labels, properties| {
        let mut labels: Vec<String> = labels
            .iter()
            .map(|label| tx.token_name(*label).unwrap())
            .collect();
        labels.sort();
        nodes.push((id.as_u64(), labels, named(properties)));
        Ok(())
    })
    .unwrap();
    let mut relationships = Vec::new();
    tx.visit_relationships(|id, record| {
        let ends = vec![
            tx.token_name(record.rel_type).unwrap(),
            record.source.as_u64().to_string(),
            record.target.as_u64().to_string(),
        ];
        relationships.push((id.as_u64(), ends, named(record.properties)));
        Ok(())
    })
    .unwrap();
    (nodes, relationships, tx.next_ids().unwrap())
}

fn same_elements(left: &Elements, right: &Elements) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(a, b)| {
            a.0 == b.0
                && a.1 == b.1
                && a.2.len() == b.2.len()
                && a.2
                    .iter()
                    .zip(&b.2)
                    .all(|(x, y)| x.0 == y.0 && same_bits(x.1, y.1))
        })
}

fn export_text(db: &Database) -> String {
    let mut out = Vec::new();
    db.export(&mut out).unwrap();
    String::from_utf8(out).unwrap()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(test_scale::cases(48)))]

    #[test]
    fn a_random_graph_imports_as_it_was_exported(
        ops in prop::collection::vec(op(), 0..60),
        indexes in 0u8..16,
    ) {
        let db = Database::in_memory().unwrap();
        build(&db, &ops, indexes);
        let dump = export_text(&db);
        let copy = Database::in_memory().unwrap();
        copy.import(dump.as_bytes()).unwrap();
        prop_assert_eq!(&export_text(&copy), &dump);
        let (nodes, relationships, next) = elements(&db);
        let (copied_nodes, copied_relationships, copied_next) = elements(&copy);
        prop_assert!(same_elements(&nodes, &copied_nodes));
        prop_assert!(same_elements(&relationships, &copied_relationships));
        prop_assert_eq!(next, copied_next);
        let violations = copy.graph().read().unwrap().check_integrity().unwrap();
        prop_assert!(violations.is_empty(), "{:?}", violations);
        let fresh = copy.execute("INSERT (n) RETURN id(n)", &crate::Parameters::new()).unwrap();
        prop_assert_eq!(
            fresh.rows(),
            [vec![crate::Value::Integer(i64::try_from(next.0).unwrap())]]
        );
    }
}
