mod support;

use rhizodb::ErrorKind;
use rhizodb::Value;
use support::*;

fn graph() -> rhizodb::Database {
    let db = db();
    run(
        &db,
        "INSERT (a:P {k: 1}), (b:P {k: 2}), (c:P {k: 3}), \
         (a)-[:T]->(b), (a)-[:T]->(c), (b)-[:T]->(c)",
    );
    db
}

#[test]
fn a_call_subquery_runs_once_per_row_with_imported_variables() {
    let db = graph();
    for query in [
        "MATCH (p:P) CALL { WITH p MATCH (p)-[:T]->(q) RETURN q.k AS qk } RETURN p.k, qk",
        "MATCH (p:P) CALL (p) { MATCH (p)-[:T]->(q) RETURN q.k AS qk } RETURN p.k, qk",
        "MATCH (p:P) CALL (*) { MATCH (p)-[:T]->(q) RETURN q.k AS qk } RETURN p.k, qk",
    ] {
        let mut rows = run(&db, query);
        rows.sort_by_key(|row| format!("{row:?}"));
        assert_eq!(
            rows,
            [[int(1), int(2)], [int(1), int(3)], [int(2), int(3)]],
            "{query}"
        );
    }
}

#[test]
fn a_call_subquery_can_aggregate_per_row() {
    let db = graph();
    let rows = run(
        &db,
        "MATCH (p:P) CALL (p) { MATCH (p)-[:T]->(q) RETURN count(q) AS c } RETURN p.k, c ORDER BY p.k",
    );
    assert_eq!(rows, [[int(1), int(2)], [int(2), int(1)], [int(3), int(0)]]);
}

#[test]
fn optional_call_keeps_a_row_whose_subquery_gives_none() {
    let db = graph();
    let rows = run(
        &db,
        "MATCH (p:P) OPTIONAL CALL (p) { MATCH (p)-[:T]->(q) RETURN q.k AS qk } \
         RETURN p.k, qk ORDER BY p.k, qk",
    );
    assert_eq!(
        rows,
        [
            [int(1), int(2)],
            [int(1), int(3)],
            [int(2), int(3)],
            [int(3), Value::Null]
        ]
    );
}

#[test]
fn a_call_subquery_without_imports_sees_no_outer_variable() {
    let db = graph();
    let rows = run(
        &db,
        "MATCH (p:P) CALL { MATCH (p)-->(q) RETURN q } RETURN count(*) AS n",
    );
    assert_eq!(
        rows,
        [[int(9)]],
        "an inner p that is not imported is a new variable"
    );
    assert_eq!(
        error_kind(&db, "MATCH (p:P) CALL () { RETURN p.k AS k } RETURN k"),
        ErrorKind::UndefinedVariable
    );
    let rows = run(
        &db,
        "UNWIND [1, 2] AS x CALL { MATCH (p:P) RETURN count(p) AS c } RETURN x, c ORDER BY x",
    );
    assert_eq!(rows, [[int(1), int(3)], [int(2), int(3)]]);
}

#[test]
fn a_call_subquery_unions_its_branches() {
    let db = db();
    let rows = run(
        &db,
        "UNWIND [1, 2] AS x CALL (x) { RETURN x AS y UNION ALL RETURN x * 10 AS y UNION ALL RETURN x AS y } \
         RETURN x, y ORDER BY x, y",
    );
    assert_eq!(
        ints(
            &rows
                .iter()
                .map(|row| vec![row[1].clone()])
                .collect::<Vec<_>>()
        ),
        [1, 1, 10, 2, 2, 20]
    );
    let rows = run(
        &db,
        "UNWIND [1, 2] AS x CALL (x) { RETURN x AS y UNION RETURN x AS y } RETURN x, y ORDER BY x",
    );
    assert_eq!(rows, [[int(1), int(1)], [int(2), int(2)]]);
}

#[test]
fn a_call_subquery_cannot_rebind_an_outer_variable() {
    let db = db();
    assert_eq!(
        error_kind(&db, "UNWIND [1] AS x CALL { RETURN 2 AS x } RETURN x"),
        ErrorKind::VariableAlreadyBound
    );
}

#[test]
fn a_call_subquery_must_return_and_cannot_update() {
    let db = graph();
    assert_eq!(
        error_kind(&db, "MATCH (p:P) CALL (p) { MATCH (p)-->(q) } RETURN p"),
        ErrorKind::InvalidClauseComposition
    );
    assert_eq!(
        error_kind(
            &db,
            "MATCH (p:P) CALL (p) { INSERT (:X) RETURN 1 AS one } RETURN p"
        ),
        ErrorKind::InvalidClauseComposition
    );
}

#[test]
fn union_branches_of_a_call_subquery_must_return_the_same_columns() {
    let db = db();
    assert_eq!(
        error_kind(&db, "CALL { RETURN 1 AS a UNION RETURN 1 AS b } RETURN a"),
        ErrorKind::DifferentColumnsInUnion
    );
}

#[test]
fn a_call_subquery_nests_and_feeds_later_clauses() {
    let db = graph();
    let rows = run(
        &db,
        "MATCH (p:P) CALL (p) { CALL (p) { MATCH (p)-->(q) RETURN q } RETURN collect(q.k) AS ks } \
         WITH p, ks WHERE size(ks) > 0 RETURN p.k, ks ORDER BY p.k",
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1], [int(2), list([int(3)])]);
}

#[test]
fn a_call_subquery_inside_exists_sees_the_outer_row() {
    let db = graph();
    let rows = run(
        &db,
        "MATCH (p:P) WHERE EXISTS { CALL (p) { MATCH (p)-->(q) RETURN q } } RETURN p.k ORDER BY p.k",
    );
    assert_eq!(ints(&rows), [1, 2]);
}
