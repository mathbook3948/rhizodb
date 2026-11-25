mod support;

use rhizodb::ErrorKind;
use support::*;

fn graph() -> rhizodb::Database {
    let db = db();
    run(
        &db,
        "INSERT (a:P {k: 1}), (b:P {k: 2}), (c:P {k: 3}), \
         (a)-[:T]->(b), (a)-[:T]->(c), (b)-[:T]->(c), (a)-[:U]->(a)",
    );
    db
}

#[test]
fn count_of_a_pattern_counts_its_matches_from_each_row() {
    let db = graph();
    let rows = run(
        &db,
        "MATCH (p:P) RETURN p.k AS k, COUNT { (p)-[:T]->() } AS c ORDER BY k",
    );
    assert_eq!(rows, [[int(1), int(2)], [int(2), int(1)], [int(3), int(0)]]);
}

#[test]
fn count_of_a_pattern_takes_a_where() {
    let db = graph();
    let rows = run(
        &db,
        "MATCH (p:P) RETURN p.k AS k, COUNT { (p)-->(q) WHERE q.k > 2 } AS c ORDER BY k",
    );
    assert_eq!(rows, [[int(1), int(1)], [int(2), int(1)], [int(3), int(0)]]);
}

#[test]
fn count_of_a_query_counts_the_rows_it_returns() {
    let db = graph();
    let rows = run(
        &db,
        "MATCH (p:P) RETURN p.k AS k, \
         COUNT { MATCH (p)-->(q) RETURN DISTINCT labels(q) AS l } AS c ORDER BY k",
    );
    assert_eq!(rows, [[int(1), int(1)], [int(2), int(1)], [int(3), int(0)]]);
    let rows = run(
        &db,
        "MATCH (p:P) RETURN p.k AS k, COUNT { MATCH (p)-->(q) } AS c ORDER BY k",
    );
    assert_eq!(rows, [[int(1), int(3)], [int(2), int(1)], [int(3), int(0)]]);
}

#[test]
fn count_is_usable_in_where_and_in_arithmetic() {
    let db = graph();
    let rows = run(
        &db,
        "MATCH (p:P) WHERE COUNT { (p)-->() } > 1 RETURN p.k * 10 + COUNT { (p)-->() } AS v",
    );
    assert_eq!(ints(&rows), [13]);
}

#[test]
fn count_keeps_the_variables_it_binds_to_itself() {
    let db = graph();
    assert_eq!(
        error_kind(&db, "MATCH (p:P) WITH COUNT { (p)-->(q) } AS c RETURN q"),
        ErrorKind::UndefinedVariable
    );
}

#[test]
fn count_cannot_update_the_graph() {
    let db = graph();
    assert_eq!(
        error_kind(&db, "MATCH (p:P) RETURN COUNT { INSERT (:X) } AS c"),
        ErrorKind::InvalidClauseComposition
    );
}

#[test]
fn count_and_collect_are_still_functions_without_braces() {
    let db = graph();
    let rows = run(
        &db,
        "MATCH (p:P) RETURN count(p) AS c, size(collect(p.k)) AS s",
    );
    assert_eq!(rows, [[int(3), int(3)]]);
}

#[test]
fn collect_gives_the_returned_column_as_a_list_in_order() {
    let db = graph();
    let rows = run(
        &db,
        "MATCH (p:P {k: 1}) RETURN COLLECT { MATCH (p)-[:T]->(q) RETURN q.k ORDER BY q.k DESC } AS l",
    );
    assert_eq!(rows, [[list([int(3), int(2)])]]);
    let rows = run(
        &db,
        "MATCH (p:P {k: 3}) RETURN COLLECT { MATCH (p)-[:T]->(q) RETURN q.k } AS l",
    );
    assert_eq!(rows, [[list([])]]);
}

#[test]
fn collect_keeps_nulls() {
    let db = db();
    let rows = run(
        &db,
        "RETURN COLLECT { UNWIND [1, null, 2] AS x RETURN x } AS l",
    );
    assert_eq!(rows, [[list([int(1), rhizodb::Value::Null, int(2)])]]);
}

#[test]
fn collect_needs_a_return_of_one_column() {
    let db = graph();
    for query in [
        "MATCH (p:P) RETURN COLLECT { MATCH (p)-->(q) } AS l",
        "MATCH (p:P) RETURN COLLECT { MATCH (p)-->(q) RETURN q.k, q } AS l",
        "MATCH (p:P) RETURN COLLECT { (p)-->(q) } AS l",
    ] {
        let kind = error_kind(&db, query);
        assert!(
            matches!(
                kind,
                ErrorKind::InvalidClauseComposition | ErrorKind::Syntax(_)
            ),
            "{query}: {kind:?}"
        );
    }
}

#[test]
fn reduce_folds_a_list_from_left_to_right() {
    let db = db();
    assert_eq!(
        single(
            &db,
            "RETURN reduce(acc = 0, x IN [1, 2, 3] | acc * 10 + x) AS v"
        ),
        int(123)
    );
    assert_eq!(
        single(&db, "RETURN reduce(s = '', x IN ['a', 'b'] | s + x) AS v"),
        string("ab")
    );
    assert_eq!(
        single(&db, "RETURN reduce(acc = 7, x IN [] | acc + x) AS v"),
        int(7)
    );
}

#[test]
fn reduce_over_null_is_null() {
    let db = db();
    assert_eq!(
        single(&db, "RETURN reduce(acc = 0, x IN null | acc + x) AS v"),
        rhizodb::Value::Null
    );
}

#[test]
fn reduce_sees_outer_variables_and_keeps_its_own() {
    let db = db();
    let rows = run(
        &db,
        "UNWIND [1, 2] AS k RETURN reduce(acc = k, x IN range(1, 3) | acc + x * k) AS v",
    );
    assert_eq!(ints(&rows), [7, 14]);
    assert_eq!(
        error_kind(&db, "RETURN reduce(acc = 0, x IN [1] | acc + x) AS v, x"),
        ErrorKind::UndefinedVariable
    );
}

#[test]
fn reduce_body_cannot_aggregate() {
    let db = db();
    assert_eq!(
        error_kind(
            &db,
            "UNWIND [1] AS k RETURN reduce(acc = 0, x IN [1] | count(x)) AS v"
        ),
        ErrorKind::InvalidAggregation
    );
}

#[test]
fn reduce_reports_a_type_error_of_its_body_before_reading_data() {
    let db = db();
    assert_eq!(
        error_kind(&db, "RETURN reduce(acc = 0, x IN [1, 2] | acc + true) AS v"),
        ErrorKind::InvalidArgumentType
    );
}

#[test]
fn reduce_is_still_a_name_without_an_accumulator() {
    let db = db();
    assert_eq!(
        error_kind(&db, "RETURN reduce(1) AS v"),
        ErrorKind::UnknownFunction
    );
}

#[test]
fn value_gives_the_one_value_its_query_returns() {
    let db = graph();
    assert_eq!(single(&db, "RETURN VALUE { RETURN 1 } AS v"), int(1));
    assert_eq!(
        run(
            &db,
            "MATCH (p:P) RETURN p.k, VALUE { MATCH (p)-[:T]->(q) RETURN max(q.k) } AS m ORDER BY p.k"
        ),
        [
            [int(1), int(3)],
            [int(2), int(3)],
            [int(3), rhizodb::Value::Null]
        ]
    );
    assert_eq!(
        single(&db, "RETURN VALUE { MATCH (z:Nope) RETURN z.k } AS v"),
        rhizodb::Value::Null
    );
    assert_eq!(
        ints(&run(
            &db,
            "MATCH (p:P) WHERE VALUE { RETURN p.k > 1 } RETURN p.k ORDER BY p.k"
        )),
        [2, 3]
    );
    assert_eq!(single(&db, "LET v = VALUE { RETURN 5 } RETURN v"), int(5));
}

#[test]
fn value_fails_when_its_query_returns_more_than_one_row() {
    let db = graph();
    assert_eq!(
        error_kind(
            &db,
            "MATCH (p:P {k: 1}) RETURN VALUE { MATCH (p)-[:T]->(q) RETURN q.k } AS v"
        ),
        ErrorKind::CardinalityViolation
    );
    assert_eq!(
        run(
            &db,
            "MATCH (p:P) WHERE p.k > 1 RETURN VALUE { MATCH (p)-[:T]->(q) RETURN q.k } AS v ORDER BY v"
        ),
        [[int(3)], [rhizodb::Value::Null]]
    );
}

#[test]
fn value_takes_a_query_of_one_column_that_does_not_update() {
    let db = graph();
    for query in [
        "RETURN VALUE { RETURN 1 AS a, 2 AS b } AS v",
        "RETURN VALUE { MATCH (p) } AS v",
        "RETURN VALUE { INSERT (:X) RETURN 1 } AS v",
    ] {
        assert_eq!(
            error_kind(&db, query),
            ErrorKind::InvalidClauseComposition,
            "{query}"
        );
    }
    assert_eq!(single(&db, "WITH 2 AS value RETURN value"), int(2));
}
