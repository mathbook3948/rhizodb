mod support;

use rhizodb::ErrorKind;
use rhizodb::SyntaxErrorKind;
use support::*;

fn sorted(db: &rhizodb::Database, query: &str) -> Vec<i64> {
    let mut values = ints(&run(db, query));
    values.sort_unstable();
    values
}

#[test]
fn except_keeps_the_rows_of_the_first_query_that_the_second_lacks() {
    let db = db();
    let left = "UNWIND [1, 1, 2, 3, 3, 3] AS x RETURN x";
    let right = "UNWIND [1, 3] AS x RETURN x";
    assert_eq!(sorted(&db, &format!("{left} EXCEPT {right}")), [2]);
    assert_eq!(sorted(&db, &format!("{left} EXCEPT DISTINCT {right}")), [2]);
    assert_eq!(
        sorted(&db, &format!("{left} EXCEPT ALL {right}")),
        [1, 2, 3, 3]
    );
}

#[test]
fn intersect_keeps_the_rows_in_both_queries() {
    let db = db();
    let left = "UNWIND [1, 1, 2, 3, 3, 3] AS x RETURN x";
    let right = "UNWIND [1, 1, 1, 3, 4] AS x RETURN x";
    assert_eq!(sorted(&db, &format!("{left} INTERSECT {right}")), [1, 3]);
    assert_eq!(
        sorted(&db, &format!("{left} INTERSECT ALL {right}")),
        [1, 1, 3]
    );
}

#[test]
fn otherwise_gives_the_second_query_only_when_the_first_gives_nothing() {
    let db = db();
    assert_eq!(
        sorted(&db, "UNWIND [1, 2] AS x RETURN x OTHERWISE RETURN 9 AS x"),
        [1, 2]
    );
    assert_eq!(
        sorted(
            &db,
            "UNWIND [] AS x RETURN x OTHERWISE UNWIND [] AS x RETURN x OTHERWISE RETURN 9 AS x"
        ),
        [9]
    );
}

#[test]
fn set_operators_chain_from_the_left_and_compare_null_as_equal() {
    let db = db();
    let rows = run(
        &db,
        "UNWIND [1, 2, 3, null] AS x RETURN x EXCEPT RETURN 2 AS x EXCEPT RETURN null AS x",
    );
    let mut values = ints(&rows);
    values.sort_unstable();
    assert_eq!(values, [1, 3]);
}

#[test]
fn set_operators_cannot_be_mixed_and_need_the_same_columns() {
    let db = db();
    assert_eq!(
        error_kind(
            &db,
            "RETURN 1 AS x EXCEPT RETURN 1 AS x UNION RETURN 2 AS x"
        ),
        ErrorKind::Syntax(SyntaxErrorKind::MixedUnion)
    );
    assert_eq!(
        error_kind(&db, "RETURN 1 AS x INTERSECT RETURN 1 AS y"),
        ErrorKind::DifferentColumnsInUnion
    );
}

#[test]
fn set_operators_work_inside_a_call_subquery() {
    let db = db();
    let rows = run(
        &db,
        "UNWIND [1, 2] AS k CALL (k) { UNWIND range(1, 3) AS x RETURN x EXCEPT RETURN k AS x } \
         RETURN k, x ORDER BY k, x",
    );
    assert_eq!(
        rows,
        [
            [int(1), int(2)],
            [int(1), int(3)],
            [int(2), int(1)],
            [int(2), int(3)]
        ]
    );
    let rows = run(
        &db,
        "UNWIND [0, 2] AS k OPTIONAL CALL (k) { UNWIND range(1, k) AS x RETURN x INTERSECT RETURN 1 AS x } \
         RETURN k, x ORDER BY k",
    );
    assert_eq!(rows, [[int(0), rhizodb::Value::Null], [int(2), int(1)]]);
}
