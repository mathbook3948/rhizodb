mod support;

use rhizodb::ErrorKind;
use rhizodb::Value;
use support::*;

fn chain() -> rhizodb::Database {
    let db = db();
    run(
        &db,
        "INSERT (:N {k: 1})-[:T]->(:N {k: 2})-[:U]->(:N {k: 3})",
    );
    db
}

fn pairs(db: &rhizodb::Database, query: &str) -> Vec<(i64, i64)> {
    run(db, query)
        .iter()
        .map(|row| match row.as_slice() {
            [Value::Integer(from), Value::Integer(to)] => (*from, *to),
            other => panic!("expected two integers, got {other:?}"),
        })
        .collect()
}

#[test]
fn each_edge_form_matches_its_orientations() {
    let db = chain();
    let forward = &[(1, 2), (2, 3)][..];
    let backward = &[(2, 1), (3, 2)][..];
    let both = &[(1, 2), (2, 1), (2, 3), (3, 2)][..];
    for (edge, expected) in [
        ("->", forward),
        ("-[]->", forward),
        ("~>", forward),
        ("~[]~>", forward),
        ("<-", backward),
        ("<~", backward),
        ("<~[]~", backward),
        ("-", both),
        ("<->", both),
        ("<-[]->", both),
        ("~", &[]),
        ("~[]~", &[]),
    ] {
        let query = format!("MATCH (a){edge}(b) RETURN a.k, b.k ORDER BY a.k, b.k");
        assert_eq!(pairs(&db, &query), expected, "{edge}");
    }
}

#[test]
fn an_undirected_edge_pattern_takes_labels_variables_and_properties() {
    let db = chain();
    assert!(run(&db, "MATCH (a)~[r:T|U {k: 1}]~(b) RETURN r").is_empty());
    let rows = pairs(&db, "MATCH (a)<~[r:T]~(b) RETURN a.k, b.k");
    assert_eq!(rows, [(2, 1)]);
    let rows = pairs(&db, "MATCH (a)~[r IS !T]~>(b) RETURN a.k, b.k");
    assert_eq!(rows, [(2, 3)]);
}

#[test]
fn a_variable_length_undirected_edge_matches_only_zero_hops() {
    let db = chain();
    let rows = pairs(&db, "MATCH (a {k: 1})~[*0..2]~(b) RETURN a.k, b.k");
    assert_eq!(rows, [(1, 1)]);
    assert!(run(&db, "MATCH (a)~[*1..]~(b) RETURN a").is_empty());
    let rows = run(&db, "MATCH p = (a {k: 2})~[r*0..]~(b) RETURN length(p), r");
    assert_eq!(rows, [vec![int(0), list([])]]);
}

#[test]
fn a_variable_length_edge_that_admits_undirected_edges_follows_its_direction() {
    let db = chain();
    let rows = pairs(
        &db,
        "MATCH (a {k: 3})<~[*1..2]~(b) RETURN a.k, b.k ORDER BY b.k",
    );
    assert_eq!(rows, [(3, 1), (3, 2)]);
    let rows = pairs(
        &db,
        "MATCH (a {k: 1})~[:T|U*]~>(b) RETURN a.k, b.k ORDER BY b.k",
    );
    assert_eq!(rows, [(1, 2), (1, 3)]);
}

#[test]
fn an_undirected_edge_works_in_optional_match_and_predicates() {
    let db = chain();
    let rows = run(
        &db,
        "MATCH (n {k: 1}) OPTIONAL MATCH (n)~(m) RETURN n.k, m.k",
    );
    assert_eq!(rows, [vec![int(1), Value::Null]]);
    let rows = run(&db, "MATCH (n) WHERE (n)~() RETURN n");
    assert!(rows.is_empty());
    let rows = run(&db, "MATCH (n) WHERE (n)<~() RETURN n.k ORDER BY n.k");
    assert_eq!(ints(&rows), [2, 3]);
    let rows = run(
        &db,
        "MATCH (n) RETURN n.k, size([(n)~>(m) | m]) AS out ORDER BY n.k",
    );
    assert_eq!(
        rows,
        [
            vec![int(1), int(1)],
            vec![int(2), int(1)],
            vec![int(3), int(0)]
        ]
    );
}

#[test]
fn a_pattern_that_creates_rejects_an_edge_that_may_be_undirected() {
    let db = chain();
    for query in [
        "INSERT ()~[:T]~()",
        "INSERT ()<~[:T]~()",
        "INSERT ()~[:T]~>()",
        "INSERT ()<-[:T]->()",
        "MATCH (a {k: 1}), (b {k: 3}) MERGE (a)~[:T]~(b)",
        "MATCH (a {k: 1}), (b {k: 3}) MERGE (a)~[:T]~>(b)",
    ] {
        assert_eq!(
            error_kind(&db, query),
            ErrorKind::RequiresDirectedRelationship,
            "{query}"
        );
    }
}

#[test]
fn merge_still_takes_an_edge_in_either_direction() {
    let db = chain();
    let rows = run(
        &db,
        "MATCH (a {k: 2}), (b {k: 1}) MERGE (a)<-[r:T]->(b) RETURN type(r)",
    );
    assert_eq!(rows, [vec![string("T")]]);
    let rows = run(&db, "MATCH ()-[r]->() RETURN count(r)");
    assert_eq!(ints(&rows), [2]);
}

#[test]
fn a_query_that_expands_nodes_again_after_adding_relationships_sees_them() {
    let db = db();
    run(
        &db,
        "UNWIND range(0, 39) AS i INSERT (:N {k: i})-[:T]->(:M), (:N {k: 100 + i})",
    );
    let rows = run(
        &db,
        "MATCH (a:N)-[:T]->() WITH a MATCH (b:N {k: a.k + 100}) INSERT (a)-[:T]->(b) \
         WITH DISTINCT a MATCH (a)-[:T]->(c) MATCH (a)-[]-(d) \
         RETURN count(c), count(d)",
    );
    assert_eq!(rows, [vec![Value::Integer(160), Value::Integer(160)]]);
    let rows = run(
        &db,
        "MATCH (a:N)-[:T]->(c) MATCH (a)-[]-(d) RETURN count(c), count(d)",
    );
    assert_eq!(rows, [vec![Value::Integer(160), Value::Integer(160)]]);
}
