mod support;

use std::collections::BTreeMap;

use rhizodb::Database;
use rhizodb::Parameters;
use rhizodb::Value;
use support::*;

fn shown(db: &Database, query: &str) -> String {
    single(db, query).to_string()
}

fn shown_row(db: &Database, query: &str) -> Vec<String> {
    let rows = run(db, query);
    assert_eq!(rows.len(), 1, "{query}");
    rows[0].iter().map(Value::to_string).collect()
}

fn echo(db: &Database, value: Value) -> Value {
    let parameters = Parameters::from([("v", value)]);
    db.execute("RETURN $v", &parameters).unwrap().rows()[0][0].clone()
}

#[test]
fn scalars_display_as_gql_literals() {
    assert_eq!(
        shown_row(
            &db(),
            "RETURN null, true, false, 0, -7, 9223372036854775807, -9223372036854775808, 'text', ''"
        ),
        [
            "null",
            "true",
            "false",
            "0",
            "-7",
            "9223372036854775807",
            "-9223372036854775808",
            "'text'",
            "''"
        ]
    );
}

#[test]
fn a_float_never_reads_as_an_integer() {
    assert_eq!(
        shown_row(
            &db(),
            "RETURN 1.0, 1.5, -2.25, 0.0, -0.0, 1e10, 1.5e-5, 123456.0, 1e7"
        ),
        [
            "1.0", "1.5", "-2.25", "0.0", "-0.0", "1.0E10", "1.5E-5", "123456.0", "1.0E7"
        ]
    );
    assert_eq!(shown(&db(), "RETURN 1"), "1");
    assert_eq!(shown(&db(), "RETURN 1.0"), "1.0");
}

#[test]
fn float_specials_display_as_to_string_writes_them() {
    let db = db();
    for (value, text) in [
        (f64::NAN, "NaN"),
        (f64::INFINITY, "Infinity"),
        (f64::NEG_INFINITY, "-Infinity"),
        (f64::MAX, "1.7976931348623157E308"),
        (f64::MIN_POSITIVE, "2.2250738585072014E-308"),
    ] {
        assert_eq!(Value::Float(value).to_string(), text);
        let parameters = Parameters::from([("v", Value::Float(value))]);
        let result = db.execute("RETURN toString($v)", &parameters).unwrap();
        assert_eq!(result.rows()[0][0], string(text));
    }
}

#[test]
fn a_string_is_quoted_and_escaped() {
    let db = db();
    let cases = [
        ("plain", "'plain'"),
        ("it's", r"'it\'s'"),
        (r#"say "hi""#, r#"'say "hi"'"#),
        (r"back\slash", r"'back\\slash'"),
        ("a\nb\rc\td", r"'a\nb\rc\td'"),
        ("\u{8}\u{c}", r"'\b\f'"),
        (
            "\u{0}\u{1}\u{1f}\u{7f}\u{85}",
            r"'\u0000\u0001\u001f\u007f\u0085'",
        ),
        ("café 日本 😀", "'café 日本 😀'"),
        ("null", "'null'"),
        ("2024-02-29", "'2024-02-29'"),
    ];
    for (text, expected) in cases {
        let value = Value::String(text.to_owned());
        assert_eq!(value.to_string(), expected);
        assert_eq!(
            single(&db, &format!("RETURN {expected}")),
            value,
            "the display of {text:?} reads back as the same string"
        );
    }
}

#[test]
fn bytes_display_as_a_hex_byte_string() {
    let db = db();
    assert_eq!(echo(&db, Value::Bytes(vec![])).to_string(), "X''");
    assert_eq!(
        echo(&db, Value::Bytes(vec![0, 15, 16, 255])).to_string(),
        "X'000f10ff'"
    );
}

#[test]
fn lists_and_maps_nest() {
    let db = db();
    assert_eq!(
        shown_row(
            &db,
            "RETURN [], {}, [1, 'a', null], {b: 2, a: 1}, [[1], [[]], {}], {k: [1, {z: 'x'}], e: []}"
        ),
        [
            "[]",
            "{}",
            "[1, 'a', null]",
            "{a: 1, b: 2}",
            "[[1], [[]], {}]",
            "{e: [], k: [1, {z: 'x'}]}"
        ]
    );
}

#[test]
fn a_map_key_that_is_not_a_plain_name_is_quoted_with_backticks() {
    let db = db();
    let keys = BTreeMap::from([
        ("first name".to_owned(), int(1)),
        ("a`b".to_owned(), int(2)),
        ("1st".to_owned(), int(3)),
        (String::new(), int(4)),
        ("match".to_owned(), int(5)),
        ("_ok9".to_owned(), int(6)),
        ("名前".to_owned(), int(7)),
    ]);
    let value = Value::Map(keys);
    let text = value.to_string();
    assert_eq!(
        text,
        "{``: 4, `1st`: 3, _ok9: 6, `a``b`: 2, `first name`: 1, match: 5, 名前: 7}"
    );
    assert_eq!(single(&db, &format!("RETURN {text}")), value);
}

#[test]
fn what_displays_as_a_literal_reads_back_as_the_same_value() {
    let db = db();
    let value = Value::List(vec![
        int(-3),
        float(2.5),
        boolean(false),
        Value::Null,
        string("it's \\ \"q\"\n"),
        Value::Map(BTreeMap::from([
            ("a b".to_owned(), Value::List(vec![])),
            ("c".to_owned(), Value::Map(BTreeMap::new())),
        ])),
    ]);
    assert_eq!(single(&db, &format!("RETURN {value}")), value);
}

#[test]
fn temporal_values_display_as_constructor_calls() {
    assert_eq!(
        shown_row(
            &db(),
            "RETURN date('2024-02-29'), localtime('10:15:30'), time('10:15+02:00'), \
             localdatetime('2024-02-29T10:15'), datetime('2024-02-29T10:15:00Z'), \
             datetime('2024-02-29T10:15:00+01:00[Europe/Paris]'), duration('P1DT2H'), \
             duration({seconds: 0}), [date('2024-02-29')]"
        ),
        [
            "date('2024-02-29')",
            "localtime('10:15:30')",
            "time('10:15+02:00')",
            "localdatetime('2024-02-29T10:15')",
            "datetime('2024-02-29T10:15Z')",
            "datetime('2024-02-29T10:15+01:00[Europe/Paris]')",
            "duration('P1DT2H')",
            "duration('PT0S')",
            "[date('2024-02-29')]"
        ]
    );
}

#[test]
fn every_temporal_reads_back_as_the_same_value() {
    let db = db();
    let query = "RETURN [date('-0044-03-15'), date('+12345-01-01'), localtime('23:59:59.999999999'), \
                 time('00:00:00.5-08:00'), localdatetime('2024-02-29T10:15:30.000000001'), \
                 datetime('2024-02-29T10:15:30.25Z'), \
                 datetime('2024-10-27T02:30:00+01:00[Europe/Paris]'), \
                 datetime('2024-02-29T10:15:00-03:30'), duration('P1Y2M3DT4H5M6.7S'), \
                 duration('P3W'), duration({days: -2, hours: 5}), duration({seconds: -1, nanoseconds: 5})]";
    let value = single(&db, query);
    let shown = value.to_string();
    assert_eq!(single(&db, &format!("RETURN {shown}")), value, "{shown}");
    let nested = Value::Map(BTreeMap::from([("t".to_owned(), value)]));
    assert_eq!(single(&db, &format!("RETURN {nested}")), nested);
}

#[test]
fn a_node_displays_its_labels_and_properties() {
    let db = db();
    run(
        &db,
        "INSERT (:Person:Admin {name: 'Ada', born: 1815, ratio: 2.0, tags: ['a', 'b']}), \
         (:Solo), (:`Odd Label`), ({k: 'only props'}), ()",
    );
    let nodes = |label: &str| shown(&db, &format!("MATCH (n{label}) RETURN n LIMIT 1"));
    assert_eq!(
        nodes(":Person"),
        "(:Admin:Person {born: 1815, name: 'Ada', ratio: 2.0, tags: ['a', 'b']})"
    );
    assert_eq!(nodes(":Solo"), "(:Solo)");
    assert_eq!(nodes(":`Odd Label`"), "(:`Odd Label`)");
    assert_eq!(
        shown(&db, "MATCH (n) WHERE n.k IS NOT NULL RETURN n"),
        "({k: 'only props'})"
    );
    assert_eq!(
        shown(
            &db,
            "MATCH (n) WHERE size(labels(n)) = 0 AND properties(n) = {} RETURN n"
        ),
        "()"
    );
}

#[test]
fn nodes_with_different_ids_display_alike() {
    let db = db();
    run(&db, "INSERT (:A {p: 1}), (:A {p: 1})");
    let rows = run(&db, "MATCH (n:A) RETURN n");
    assert_ne!(rows[0][0], rows[1][0]);
    assert_eq!(rows[0][0].to_string(), rows[1][0].to_string());
}

#[test]
fn a_relationship_displays_its_type_and_properties() {
    let db = db();
    run(
        &db,
        "INSERT (:A)-[:KNOWS {since: 2020, weight: 0.5}]->(:B), (:C)-[:`HAS PART`]->(:D), (:E)-[:PLAIN]->(:F)",
    );
    assert_eq!(
        shown(&db, "MATCH ()-[r:KNOWS]->() RETURN r"),
        "[:KNOWS {since: 2020, weight: 0.5}]"
    );
    assert_eq!(
        shown(&db, "MATCH ()-[r:`HAS PART`]->() RETURN r"),
        "[:`HAS PART`]"
    );
    assert_eq!(shown(&db, "MATCH ()-[r:PLAIN]->() RETURN r"), "[:PLAIN]");
}

#[test]
fn a_path_displays_each_relationship_in_its_traversal_direction() {
    let db = db();
    run(
        &db,
        "INSERT (:Stop {n: 1})-[:TO {w: 1}]->(:Stop {n: 2})<-[:BACK]-(:Stop {n: 3})",
    );
    assert_eq!(
        shown(
            &db,
            "MATCH p = (:Stop {n: 1})-[:TO]->()<-[:BACK]-(:Stop {n: 3}) RETURN p"
        ),
        "<(:Stop {n: 1})-[:TO {w: 1}]->(:Stop {n: 2})<-[:BACK]-(:Stop {n: 3})>"
    );
    assert_eq!(
        shown(
            &db,
            "MATCH p = (:Stop {n: 3})-[:BACK]->()<-[:TO]-(:Stop {n: 1}) RETURN p"
        ),
        "<(:Stop {n: 3})-[:BACK]->(:Stop {n: 2})<-[:TO {w: 1}]-(:Stop {n: 1})>"
    );
    assert_eq!(
        shown(&db, "MATCH p = (:Stop {n: 2}) RETURN p"),
        "<(:Stop {n: 2})>"
    );
    assert_eq!(
        shown(&db, "MATCH p = (:Stop {n: 2})<-[:TO]-(:Stop) RETURN p"),
        "<(:Stop {n: 2})<-[:TO {w: 1}]-(:Stop {n: 1})>"
    );
}

#[test]
fn a_self_loop_points_forward() {
    let db = db();
    run(&db, "INSERT (a:Loop)-[:SELF]->(a)");
    assert_eq!(
        shown(&db, "MATCH p = (:Loop)-[:SELF]->(:Loop) RETURN p"),
        "<(:Loop)-[:SELF]->(:Loop)>"
    );
}

#[test]
fn elements_display_inside_lists_and_maps() {
    let db = db();
    run(&db, "INSERT (:A {p: 1})-[:T]->(:B)");
    assert_eq!(
        shown(
            &db,
            "MATCH (a:A)-[r:T]->(b:B) RETURN {from: a, via: [r], to: b}"
        ),
        "{from: (:A {p: 1}), to: (:B), via: [[:T]]}"
    );
}
