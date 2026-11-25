mod support;

use rhizodb::ErrorKind;
use rhizodb::Value;
use support::*;

#[test]
fn next_feeds_the_returned_columns_to_the_next_statement() {
    let db = db();
    assert_eq!(single(&db, "RETURN 1 AS a NEXT RETURN a + 1 AS b"), int(2));
    assert_eq!(
        single(
            &db,
            "UNWIND [1, 2, 3] AS k RETURN k NEXT RETURN sum(k) AS s NEXT RETURN s * 10 AS t"
        ),
        int(60)
    );
    assert_eq!(
        run(
            &db,
            "UNWIND [3, 1, 2] AS x RETURN x NEXT RETURN x ORDER BY x DESC LIMIT 2"
        ),
        [[int(3)], [int(2)]]
    );
    assert_eq!(columns(&db, "RETURN 1 AS a NEXT RETURN a AS z"), ["z"]);
}

#[test]
fn a_statement_after_next_sees_only_the_columns_before_it() {
    let db = db();
    assert_eq!(
        error_kind(&db, "UNWIND [1] AS x RETURN x AS y NEXT RETURN x"),
        ErrorKind::UndefinedVariable
    );
    assert_eq!(
        single(
            &db,
            "RETURN 1 AS a NEXT RETURN a AS a NEXT RETURN a * 3 AS c"
        ),
        int(3)
    );
    assert_eq!(
        single(&db, "RETURN 1 AS a, 2 AS b NEXT YIELD b RETURN b"),
        int(2)
    );
    assert_eq!(
        error_kind(&db, "RETURN 1 AS a, 2 AS b NEXT YIELD b RETURN a"),
        ErrorKind::UndefinedVariable
    );
    assert_eq!(
        error_kind(&db, "RETURN 1 AS a NEXT YIELD c RETURN 1 AS d"),
        ErrorKind::UndefinedVariable
    );
}

#[test]
fn next_joins_set_operations_over_the_whole_table() {
    let db = db();
    assert_eq!(
        single(
            &db,
            "UNWIND [1, 2] AS k RETURN k UNION ALL RETURN 10 AS k NEXT RETURN sum(k) AS s"
        ),
        int(13)
    );
    let mut rows = run(
        &db,
        "UNWIND [1, 2, 3] AS k RETURN k NEXT RETURN 1 AS x UNION RETURN 2 AS x",
    );
    rows.sort_by_key(|row| format!("{row:?}"));
    assert_eq!(rows, [[int(1)], [int(2)]]);
    assert_eq!(
        single(
            &db,
            "UNWIND [1, 2] AS k RETURN k NEXT RETURN k AS x EXCEPT RETURN 1 AS x NEXT RETURN collect(x) AS xs"
        ),
        list([int(2)])
    );
}

#[test]
fn a_statement_after_next_may_read_after_an_update() {
    let db = db();
    assert_eq!(
        single(
            &db,
            "INSERT (:N {v: 1}) NEXT MATCH (n:N) RETURN count(n) AS c"
        ),
        int(1)
    );
    assert_eq!(
        run(
            &db,
            "UNWIND [1, 2] AS i INSERT (:M {v: i}) RETURN i NEXT MATCH (m:M) WHERE m.v = i RETURN i, m.v ORDER BY i"
        ),
        [[int(1), int(1)], [int(2), int(2)]]
    );
    assert_eq!(
        single(
            &db,
            "MATCH (m:M) SET m.w = m.v * 2 NEXT MATCH (m:M) RETURN sum(m.w) AS s"
        ),
        int(6)
    );
}

#[test]
fn a_statement_without_return_passes_on_one_empty_row() {
    let db = db();
    assert_eq!(
        single(&db, "MATCH (n:Nope) DELETE n NEXT RETURN count(*) AS c"),
        int(1)
    );
    assert_eq!(
        single(
            &db,
            "UNWIND [1, 2, 3] AS i INSERT (:X) NEXT RETURN count(*) AS c"
        ),
        int(1)
    );
    assert_eq!(
        error_kind(&db, "MATCH (n) NEXT RETURN 1 AS x"),
        ErrorKind::InvalidClauseComposition
    );
}

#[test]
fn next_works_inside_a_call_subquery() {
    let db = db();
    assert_eq!(
        run(
            &db,
            "UNWIND [1, 2] AS i CALL (i) { RETURN i AS j NEXT RETURN j * 10 AS k } RETURN i, k ORDER BY i"
        ),
        [[int(1), int(10)], [int(2), int(20)]]
    );
    assert_eq!(
        run(
            &db,
            "UNWIND [1, 2] AS i CALL (i) { INSERT (:C {i: i}) NEXT MATCH (c:C) RETURN count(c) AS n } \
             RETURN i, n ORDER BY i"
        ),
        [[int(1), int(1)], [int(2), int(2)]]
    );
}

#[test]
fn a_failing_statement_after_next_rolls_back_the_ones_before() {
    let db = db();
    assert_eq!(
        error_kind(&db, "INSERT (:R) NEXT UNWIND [0] AS z RETURN 1 / z AS q"),
        ErrorKind::DivisionByZero
    );
    assert_eq!(single(&db, "MATCH (r:R) RETURN count(r)"), int(0));
}

#[test]
fn next_is_still_a_name() {
    let db = db();
    assert_eq!(single(&db, "WITH 1 AS next RETURN next"), int(1));
    run(&db, "INSERT (:NEXT)-[:NEXT]->(:Next)");
    assert_eq!(
        single(&db, "MATCH (:NEXT)-[r:NEXT]->() RETURN count(r) AS next"),
        int(1)
    );
    assert_eq!(
        run(&db, "MATCH (n:NEXT) RETURN n.missing AS x NEXT RETURN x"),
        [[Value::Null]]
    );
}
