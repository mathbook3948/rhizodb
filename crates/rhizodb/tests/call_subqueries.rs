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
fn a_call_subquery_that_only_reads_must_return() {
    let db = graph();
    for query in [
        "MATCH (p:P) CALL (p) { MATCH (p)-->(q) } RETURN p",
        "MATCH (p:P) CALL (p) { MATCH (p)-->(q) RETURN q }",
        "MATCH (p:P) CALL (p) { INSERT (:X) WITH p MATCH (p)-->(q) } RETURN p",
    ] {
        assert_eq!(
            error_kind(&db, query),
            ErrorKind::InvalidClauseComposition,
            "{query}"
        );
    }
}

#[test]
fn an_updating_call_subquery_runs_its_writes_once_per_row() {
    let db = graph();
    let rows = run(
        &db,
        "MATCH (p:P) CALL (p) { INSERT (:X {k: p.k * 10}) RETURN p.k * 10 AS made } \
         RETURN p.k, made ORDER BY p.k",
    );
    assert_eq!(
        rows,
        [[int(1), int(10)], [int(2), int(20)], [int(3), int(30)]]
    );
    assert_eq!(
        ints(&run(&db, "MATCH (x:X) RETURN x.k ORDER BY x.k")),
        [10, 20, 30]
    );
}

#[test]
fn a_call_subquery_without_return_keeps_each_row_once() {
    let db = graph();
    let rows = run(
        &db,
        "MATCH (p:P) CALL (p) { MATCH (p)-[:T]->(q) SET q.seen = coalesce(q.seen, 0) + 1 } \
         RETURN p.k ORDER BY p.k",
    );
    assert_eq!(ints(&rows), [1, 2, 3]);
    assert_eq!(
        run(&db, "MATCH (p:P) RETURN p.k, p.seen ORDER BY p.k"),
        [[int(1), Value::Null], [int(2), int(1)], [int(3), int(2)]]
    );
    assert_eq!(
        run(&db, "MATCH (p:P) CALL (p) { SET p.done = true }"),
        Vec::<Vec<Value>>::new()
    );
    assert_eq!(
        single(&db, "MATCH (p:P) WHERE p.done RETURN count(p)"),
        int(3)
    );
}

#[test]
fn each_row_of_an_updating_call_subquery_sees_the_writes_of_the_rows_before() {
    let db = db();
    let rows = run(
        &db,
        "UNWIND [1, 2, 3] AS i CALL (i) { MATCH (c:C) WITH count(c) AS before INSERT (:C) RETURN before } \
         RETURN i, before ORDER BY i",
    );
    assert_eq!(rows, [[int(1), int(0)], [int(2), int(1)], [int(3), int(2)]]);
    let rows = run(
        &db,
        "UNWIND [1, 2, 3, 4] AS i CALL (i) { MERGE (m:M {parity: i % 2}) RETURN m } \
         RETURN count(DISTINCT m) AS n",
    );
    assert_eq!(rows, [[int(2)]]);
}

#[test]
fn an_updating_call_subquery_takes_every_scope_form_and_optional_call() {
    let db = graph();
    for (index, query) in [
        "MATCH (p:P) CALL { WITH p SET p.a = 1 } RETURN count(*) AS c",
        "MATCH (p:P) CALL (p) { SET p.a = 2 } RETURN count(*) AS c",
        "MATCH (p:P) CALL (*) { SET p.a = 3 } RETURN count(*) AS c",
        "MATCH (p:P) OPTIONAL CALL (p) { SET p.a = 4 } RETURN count(*) AS c",
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(run(&db, query), [[int(3)]], "{query}");
        let expected = i64::try_from(index).unwrap() + 1;
        assert_eq!(
            ints(&run(&db, "MATCH (p:P) RETURN DISTINCT p.a")),
            [expected],
            "{query}"
        );
    }
    let rows = run(
        &db,
        "MATCH (p:P) OPTIONAL CALL (p) { MATCH (p)-[:T]->(q) SET q.b = true RETURN q } \
         RETURN p.k, q.k ORDER BY p.k, q.k",
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
    assert_eq!(
        error_kind(&db, "MATCH (p:P) CALL () { SET p.a = 5 } RETURN p"),
        ErrorKind::UndefinedVariable
    );
}

#[test]
fn an_updating_call_subquery_unions_and_nests() {
    let db = db();
    let rows = run(
        &db,
        "UNWIND [1, 2] AS i CALL (i) { INSERT (x:X {i: i}) RETURN x \
         UNION ALL INSERT (y:X {i: -i}) RETURN y AS x } RETURN x.i ORDER BY x.i",
    );
    assert_eq!(ints(&rows), [-2, -1, 1, 2]);
    run(
        &db,
        "MATCH (x:X) CALL (x) { CALL (x) { SET x.depth = 2 } SET x.outer = true }",
    );
    assert_eq!(
        single(
            &db,
            "MATCH (x:X) WHERE x.depth = 2 AND x.outer RETURN count(x)"
        ),
        int(4)
    );
}

#[test]
fn an_element_deleted_in_a_call_subquery_is_returned_as_it_was() {
    let db = graph();
    let rows = run(
        &db,
        "MATCH (p:P {k: 3}) CALL (p) { DETACH DELETE p } RETURN p",
    );
    assert_eq!(rows.len(), 1);
    match &rows[0][0] {
        Value::Node(node) => assert_eq!(node.property("k"), Some(&int(3))),
        other => panic!("expected a node, got {other:?}"),
    }
    assert_eq!(single(&db, "MATCH (p:P) RETURN count(p)"), int(2));
}

#[test]
fn a_failing_row_rolls_back_every_write_of_an_updating_call_subquery() {
    let db = graph();
    assert_eq!(
        error_kind(
            &db,
            "UNWIND [1, 2, 0] AS i CALL (i) { INSERT (:X {v: 1 / i}) }"
        ),
        ErrorKind::DivisionByZero
    );
    assert_eq!(single(&db, "MATCH (x:X) RETURN count(x)"), int(0));
    let mut tx = db.begin_write().unwrap();
    tx.execute("INSERT (:Y)", &rhizodb::Parameters::new())
        .unwrap();
    assert!(
        tx.execute(
            "UNWIND [1, 0] AS i CALL (i) { INSERT (:X {v: 1 / i}) }",
            &rhizodb::Parameters::new()
        )
        .is_err()
    );
    assert!(tx.commit().is_err());
    assert_eq!(
        single(&db, "MATCH (n) WHERE n:X OR n:Y RETURN count(n)"),
        int(0)
    );
}

#[test]
fn an_updating_call_subquery_is_an_updating_clause() {
    let db = graph();
    assert_eq!(
        error_kind(
            &db,
            "MATCH (p:P) CALL (p) { SET p.a = 1 } MATCH (q) RETURN q"
        ),
        ErrorKind::InvalidClauseComposition
    );
    let rows = run(
        &db,
        "MATCH (p:P) CALL (p) { SET p.a = 1 } WITH p MATCH (q:P) WHERE q.a = 1 RETURN count(*) AS c",
    );
    assert_eq!(rows, [[int(9)]]);
    let tx = db.begin_read().unwrap();
    assert_eq!(
        tx.execute(
            "MATCH (p:P) CALL (p) { SET p.a = 2 } RETURN p.a",
            &rhizodb::Parameters::new()
        )
        .unwrap_err()
        .kind(),
        ErrorKind::ReadOnlyTransaction
    );
}

#[test]
fn a_subquery_inside_an_expression_cannot_update() {
    let db = graph();
    for query in [
        "MATCH (p:P) RETURN EXISTS { CALL (p) { SET p.a = 1 } } AS e",
        "MATCH (p:P) RETURN COUNT { CALL (p) { INSERT (:X) RETURN 1 AS one } RETURN one } AS c",
        "MATCH (p:P) RETURN COLLECT { CALL (p) { DELETE p } RETURN 1 } AS c",
    ] {
        assert_eq!(
            error_kind(&db, query),
            ErrorKind::InvalidClauseComposition,
            "{query}"
        );
    }
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

#[test]
fn an_uncorrelated_call_inside_a_hidden_scope_reads_none_of_the_hidden_variables() {
    let db = graph();
    for query in [
        "MATCH (n) RETURN DISTINCT n.k AS k ORDER BY COUNT { CALL { MATCH (n) RETURN n AS m } RETURN m }, k",
        "MATCH (n) RETURN DISTINCT n.k AS k ORDER BY COUNT { CALL () { MATCH (n) RETURN n AS m } RETURN m }, k",
        "MATCH (n) RETURN DISTINCT n.k AS k \
         ORDER BY COUNT { CALL { WITH 1 AS one MATCH (n) RETURN n AS m } RETURN m }, k",
    ] {
        assert_eq!(
            run(&db, query),
            [vec![int(1)], vec![int(2)], vec![int(3)]],
            "{query}"
        );
    }
    for query in [
        "MATCH (n) RETURN DISTINCT n.k AS k ORDER BY COUNT { CALL { WITH n MATCH (n)-->(m) RETURN m } RETURN m }",
        "MATCH (n) RETURN DISTINCT n.k AS k ORDER BY COUNT { CALL (n) { MATCH (n)-->(m) RETURN m } RETURN m }",
    ] {
        assert_eq!(
            error_kind(&db, query),
            ErrorKind::UndefinedVariable,
            "{query}"
        );
    }
}
