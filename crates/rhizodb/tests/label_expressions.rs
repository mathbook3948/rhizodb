mod support;

use rhizodb::ErrorKind;
use rhizodb::Value;
use support::*;

fn labeled() -> rhizodb::Database {
    let db = db();
    run(
        &db,
        "INSERT (:A {k: 1}), (:B {k: 2}), (:A:B {k: 3}), (:C {k: 4}), ({k: 5}), \
         (:A {k: 6})",
    );
    db
}

fn keys(db: &rhizodb::Database, query: &str) -> Vec<i64> {
    ints(&run(db, query))
}

#[test]
fn node_patterns_combine_labels_with_every_operator() {
    let db = labeled();
    for (pattern, expected) in [
        ("(n:A)", &[1, 3, 6][..]),
        ("(n:A&B)", &[3]),
        ("(n:A|B)", &[1, 2, 3, 6]),
        ("(n:!A)", &[2, 4, 5]),
        ("(n:A&!B)", &[1, 6]),
        ("(n:!(A|B))", &[4, 5]),
        ("(n:%)", &[1, 2, 3, 4, 6]),
        ("(n:!%)", &[5]),
        ("(n IS B|C)", &[2, 3, 4]),
        ("(n:A:B)", &[3]),
        ("(n:Missing|C)", &[4]),
        ("(n:!Missing)", &[1, 2, 3, 4, 5, 6]),
    ] {
        let query = format!("MATCH {pattern} RETURN n.k ORDER BY n.k");
        assert_eq!(keys(&db, &query), expected, "{pattern}");
    }
}

#[test]
fn label_expressions_combine_with_property_maps() {
    let db = labeled();
    assert_eq!(keys(&db, "MATCH (n:A|B {k: 3}) RETURN n.k"), [3]);
    assert_eq!(keys(&db, "MATCH (n IS !A {k: 2}) RETURN n.k"), [2]);
    assert!(run(&db, "MATCH (n:!A {k: 1}) RETURN n.k").is_empty());
}

fn typed() -> rhizodb::Database {
    let db = db();
    run(
        &db,
        "INSERT (a:N {k: 0})-[:T {k: 1}]->(b:N {k: 1}), (b)-[:U {k: 2}]->(c:N {k: 2}), \
         (c)-[:V {k: 3}]->(:N {k: 3})",
    );
    db
}

#[test]
fn relationship_patterns_test_the_type_as_the_only_label() {
    let db = typed();
    for (pattern, expected) in [
        ("[r:T|U]", &[1, 2][..]),
        ("[r:!T]", &[2, 3]),
        ("[r:T&U]", &[]),
        ("[r:%]", &[1, 2, 3]),
        ("[r:!%]", &[]),
        ("[r IS V|Missing]", &[3]),
        ("[r:T|:V]", &[1, 3]),
    ] {
        let query = format!("MATCH ()-{pattern}->() RETURN r.k ORDER BY r.k");
        assert_eq!(keys(&db, &query), expected, "{pattern}");
    }
}

#[test]
fn a_label_expression_constrains_every_hop_of_a_variable_length_relationship() {
    let db = typed();
    let ends = keys(
        &db,
        "MATCH ({k: 0})-[:T|U*1..3]->(m) RETURN m.k ORDER BY m.k",
    );
    assert_eq!(ends, [1, 2]);
    let ends = keys(&db, "MATCH ({k: 0})-[:!U*]->(m) RETURN m.k");
    assert_eq!(ends, [1]);
}

#[test]
fn label_predicates_take_label_expressions() {
    let db = labeled();
    assert_eq!(
        keys(&db, "MATCH (n) WHERE n:A|C RETURN n.k ORDER BY n.k"),
        [1, 3, 4, 6]
    );
    assert_eq!(
        keys(
            &db,
            "MATCH (n) WHERE n IS LABELED A & !B RETURN n.k ORDER BY n.k"
        ),
        [1, 6]
    );
    assert_eq!(
        keys(
            &db,
            "MATCH (n) WHERE n IS NOT LABELED % RETURN n.k ORDER BY n.k"
        ),
        [5]
    );
    let rows = run(
        &db,
        "MATCH (n {k: 3}) RETURN n:A&B AS both, n IS LABELED C AS c, n IS NOT LABELED C AS notC",
    );
    assert_eq!(rows, [vec![boolean(true), boolean(false), boolean(true)]]);
}

#[test]
fn a_label_predicate_on_a_relationship_reads_its_type() {
    let db = typed();
    let rows = run(
        &db,
        "MATCH ()-[r]->() RETURN r.k, r IS LABELED T|U AS tu, r:!V AS notV ORDER BY r.k",
    );
    assert_eq!(
        rows,
        [
            vec![int(1), boolean(true), boolean(true)],
            vec![int(2), boolean(true), boolean(true)],
            vec![int(3), boolean(false), boolean(false)],
        ]
    );
}

#[test]
fn a_label_predicate_on_null_is_null() {
    let db = labeled();
    let rows = run(
        &db,
        "OPTIONAL MATCH (n:Missing) RETURN n:A|B AS a, n IS NOT LABELED A AS b",
    );
    assert_eq!(rows, [vec![Value::Null, Value::Null]]);
}

#[test]
fn a_label_predicate_on_a_value_that_is_not_an_element_fails() {
    let db = db();
    assert_eq!(
        error_kind(&db, "RETURN 1 IS LABELED A"),
        ErrorKind::InvalidArgumentType
    );
    assert_eq!(
        error_kind(&db, "UNWIND [1] AS x RETURN x:A|B"),
        ErrorKind::TypeMismatch
    );
}

#[test]
fn a_comprehension_filter_ends_at_a_pipe_outside_parentheses() {
    let db = labeled();
    let rows = run(
        &db,
        "MATCH (n) WITH collect(n) AS ns \
         RETURN [x IN ns WHERE x:A | x.k] AS a, [x IN ns WHERE (x:B|C) | x.k] AS bc",
    );
    let [row] = rows.as_slice() else {
        panic!("expected one row, got {rows:?}");
    };
    let sorted = |value: &Value| match value {
        Value::List(items) => {
            let mut keys: Vec<i64> = items
                .iter()
                .map(|item| match item {
                    Value::Integer(key) => *key,
                    other => panic!("expected an integer, got {other:?}"),
                })
                .collect();
            keys.sort_unstable();
            keys
        }
        other => panic!("expected a list, got {other:?}"),
    };
    assert_eq!(sorted(&row[0]), [1, 3, 6]);
    assert_eq!(sorted(&row[1]), [2, 3, 4]);
}

#[test]
fn insert_takes_a_conjunction_of_labels() {
    let db = db();
    run(&db, "INSERT (:A&B {k: 1}), (IS C&D {k: 2})");
    let rows = run(
        &db,
        "MATCH (n) RETURN n.k, labels(n) AS labels ORDER BY n.k",
    );
    assert_eq!(
        rows,
        [
            vec![int(1), list([string("A"), string("B")])],
            vec![int(2), list([string("C"), string("D")])],
        ]
    );
}

#[test]
fn a_pattern_that_creates_rejects_any_other_label_expression() {
    let db = db();
    for query in [
        "INSERT (:A|B)",
        "INSERT (:!A)",
        "INSERT (:%)",
        "INSERT (n IS A&!B) RETURN n",
        "MERGE (n:A|B) RETURN n",
        "MATCH (a) MERGE (a)-[:T]->(:!B)",
    ] {
        assert_eq!(error_kind(&db, query), ErrorKind::NoLabelSet, "{query}");
    }
    for query in [
        "INSERT ()-[:T|U]->()",
        "INSERT ()-[:!T]->()",
        "INSERT ()-[:T&U]->()",
        "MERGE ()-[:%]->()",
    ] {
        assert_eq!(
            error_kind(&db, query),
            ErrorKind::NoSingleRelationshipType,
            "{query}"
        );
    }
}

#[test]
fn merge_matches_a_conjunction_like_a_label_list() {
    let db = labeled();
    let rows = run(&db, "MERGE (n:A&B {k: 3}) RETURN n.k");
    assert_eq!(ints(&rows), [3]);
    assert_eq!(keys(&db, "MATCH (n:A:B) RETURN count(n)"), [1]);
}

#[test]
fn a_deleted_relationship_keeps_its_type_for_a_label_expression() {
    let db = typed();
    let rows = run(
        &db,
        "MATCH ()-[r:T|U]->() DELETE r RETURN r IS LABELED T|V AS t ORDER BY t",
    );
    assert_eq!(rows, [vec![boolean(false)], vec![boolean(true)]]);
}
