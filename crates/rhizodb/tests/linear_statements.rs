mod support;

use rhizodb::ErrorKind;
use support::*;

fn chain() -> rhizodb::Database {
    let db = db();
    run(
        &db,
        "INSERT (:N {k: 1})-[:T]->(:N {k: 2})-[:T]->(:N {k: 3})",
    );
    db
}

#[test]
fn filter_keeps_only_rows_whose_condition_is_true() {
    let db = db();
    let rows = run(&db, "UNWIND [1, null, 2, 3] AS k FILTER k > 1 RETURN k");
    assert_eq!(ints(&rows), [2, 3]);
    let rows = run(&db, "UNWIND [1, 2] AS k FILTER false RETURN k");
    assert!(rows.is_empty());
}

#[test]
fn filter_keeps_every_variable_in_scope() {
    let db = db();
    let rows = run(
        &db,
        "UNWIND [1, 2] AS a UNWIND [10, 20] AS b FILTER WHERE a = 2 RETURN a + b AS s",
    );
    assert_eq!(ints(&rows), [12, 22]);
}

#[test]
fn filters_chain_like_one_conjunction() {
    let db = db();
    let rows = run(
        &db,
        "UNWIND range(1, 10) AS k FILTER k % 2 = 0 FILTER k > 4 RETURN k",
    );
    assert_eq!(ints(&rows), [6, 8, 10]);
}

#[test]
fn filter_after_aggregation_tests_the_aggregated_rows() {
    let db = db();
    let rows = run(
        &db,
        "UNWIND [1, 1, 2, 3, 3, 3] AS k WITH k, count(*) AS c FILTER c > 1 RETURN k ORDER BY k",
    );
    assert_eq!(ints(&rows), [1, 3]);
}

#[test]
fn filter_cannot_aggregate() {
    let db = db();
    assert_eq!(
        error_kind(&db, "UNWIND [1, 2] AS k FILTER count(*) > 1 RETURN k"),
        ErrorKind::InvalidAggregation
    );
}

#[test]
fn filter_takes_a_pattern_predicate() {
    let db = chain();
    let rows = run(&db, "MATCH (n) FILTER (n)-[:T]->() RETURN n.k ORDER BY n.k");
    assert_eq!(ints(&rows), [1, 2]);
    let rows = run(
        &db,
        "MATCH (n) FILTER NOT EXISTS { MATCH (n)-->(m) FILTER m.k = 2 } RETURN n.k ORDER BY n.k",
    );
    assert_eq!(ints(&rows), [2, 3]);
}

#[test]
fn filter_condition_must_be_boolean() {
    let db = db();
    assert_eq!(
        error_kind(&db, "UNWIND [1] AS k FILTER 'yes' RETURN k"),
        ErrorKind::InvalidArgumentType
    );
    assert_eq!(
        error_kind(&db, "UNWIND [1] AS k FILTER k RETURN k"),
        ErrorKind::TypeMismatch
    );
}

#[test]
fn filter_reads_only_variables_in_scope() {
    let db = db();
    assert_eq!(
        error_kind(&db, "UNWIND [1] AS k FILTER j > 0 RETURN k"),
        ErrorKind::UndefinedVariable
    );
    assert_eq!(
        error_kind(&db, "UNWIND [1] AS k WITH k AS j FILTER k > 0 RETURN j"),
        ErrorKind::UndefinedVariable
    );
}

#[test]
fn filter_cannot_end_a_query() {
    let db = db();
    assert_eq!(
        error_kind(&db, "UNWIND [1] AS k FILTER k > 0"),
        ErrorKind::InvalidClauseComposition
    );
}

#[test]
fn filter_after_an_update_sees_the_update_but_does_not_separate_it_from_a_read() {
    let db = db();
    let rows = run(
        &db,
        "INSERT (n:A {k: 1}), (:A {k: 2}) FILTER n.k = 1 RETURN n.k",
    );
    assert_eq!(ints(&rows), [1]);
    assert_eq!(
        error_kind(&db, "INSERT (n:A) FILTER true MATCH (m) RETURN m"),
        ErrorKind::InvalidClauseComposition
    );
    let rows = run(
        &db,
        "INSERT (n:B) FILTER true WITH n MATCH (m:B) RETURN count(m)",
    );
    assert_eq!(ints(&rows), [1]);
}
