use super::*;
use crate::scenario::Phase;

fn ints(rows: &[&[i64]]) -> Vec<Vec<Value>> {
    rows.iter()
        .map(|row| row.iter().copied().map(Value::Integer).collect())
        .collect()
}

fn table(columns: &[&str], rows: Vec<Vec<Value>>) -> Rows {
    Rows {
        columns: columns.iter().map(|column| column.to_string()).collect(),
        rows,
    }
}

fn expect_rows(
    columns: &[&str],
    rows: Vec<Vec<Value>>,
    ordering: Ordering,
    lists: Lists,
) -> Expected {
    Expected::Rows {
        columns: columns.iter().map(|column| column.to_string()).collect(),
        rows,
        ordering,
        lists,
    }
}

fn raised(kind: &str, detail: &str) -> Result<Rows, QueryError> {
    Err(QueryError::Raised {
        kind: kind.into(),
        detail: detail.into(),
    })
}

fn expect_error(kind: &str, phase: Phase, detail: &str) -> Expected {
    Expected::Error {
        kind: kind.into(),
        phase,
        detail: detail.into(),
    }
}

#[test]
fn any_order_accepts_a_permutation_of_the_rows() {
    let expected = expect_rows(
        &["n"],
        ints(&[&[1], &[2], &[3]]),
        Ordering::AnyOrder,
        Lists::Strict,
    );
    let actual = Ok(table(&["n"], ints(&[&[3], &[1], &[2]])));

    assert_eq!(check_outcome(&expected, &actual), Ok(()));
}

#[test]
fn in_order_rejects_a_permutation_of_the_rows() {
    let expected = expect_rows(
        &["n"],
        ints(&[&[1], &[2]]),
        Ordering::InOrder,
        Lists::Strict,
    );
    let actual = Ok(table(&["n"], ints(&[&[2], &[1]])));

    assert!(check_outcome(&expected, &actual).is_err());
}

#[test]
fn in_order_accepts_the_same_order() {
    let expected = expect_rows(
        &["n"],
        ints(&[&[1], &[2]]),
        Ordering::InOrder,
        Lists::Strict,
    );
    let actual = Ok(table(&["n"], ints(&[&[1], &[2]])));

    assert_eq!(check_outcome(&expected, &actual), Ok(()));
}

#[test]
fn any_order_counts_duplicate_rows() {
    let expected = expect_rows(
        &["n"],
        ints(&[&[1], &[1], &[2]]),
        Ordering::AnyOrder,
        Lists::Strict,
    );

    assert!(check_outcome(&expected, &Ok(table(&["n"], ints(&[&[1], &[2], &[2]])))).is_err());
    assert!(check_outcome(&expected, &Ok(table(&["n"], ints(&[&[1], &[2]])))).is_err());
    assert_eq!(
        check_outcome(&expected, &Ok(table(&["n"], ints(&[&[2], &[1], &[1]])))),
        Ok(())
    );
}

#[test]
fn rows_must_agree_on_columns() {
    let expected = expect_rows(&["n"], ints(&[&[1]]), Ordering::AnyOrder, Lists::Strict);

    assert!(check_outcome(&expected, &Ok(table(&["m"], ints(&[&[1]])))).is_err());
}

#[test]
fn a_header_only_table_expects_zero_rows() {
    let expected = expect_rows(&["n"], vec![], Ordering::AnyOrder, Lists::Strict);

    assert_eq!(check_outcome(&expected, &Ok(table(&["n"], vec![]))), Ok(()));
    assert!(check_outcome(&expected, &Ok(table(&["n"], ints(&[&[1]])))).is_err());
}

#[test]
fn list_element_order_matters_unless_the_scenario_ignores_it() {
    let list = |items: &[i64]| {
        vec![vec![Value::List(
            items.iter().copied().map(Value::Integer).collect(),
        )]]
    };
    let actual = Ok(table(&["xs"], list(&[2, 1])));

    let strict = expect_rows(&["xs"], list(&[1, 2]), Ordering::AnyOrder, Lists::Strict);
    let lenient = expect_rows(
        &["xs"],
        list(&[1, 2]),
        Ordering::AnyOrder,
        Lists::IgnoreElementOrder,
    );

    assert!(check_outcome(&strict, &actual).is_err());
    assert_eq!(check_outcome(&lenient, &actual), Ok(()));
}

#[test]
fn ignoring_list_order_keeps_row_order_when_in_order() {
    let expected = expect_rows(
        &["n"],
        ints(&[&[1], &[2]]),
        Ordering::InOrder,
        Lists::IgnoreElementOrder,
    );

    assert!(check_outcome(&expected, &Ok(table(&["n"], ints(&[&[2], &[1]])))).is_err());
}

#[test]
fn empty_expects_no_columns_and_no_rows() {
    assert_eq!(
        check_outcome(&Expected::Empty, &Ok(Rows::default())),
        Ok(())
    );
    assert!(check_outcome(&Expected::Empty, &Ok(table(&["n"], vec![]))).is_err());
    assert!(check_outcome(&Expected::Empty, &Ok(table(&[], vec![vec![]]))).is_err());
}

#[test]
fn an_error_matches_on_kind_and_detail_and_ignores_the_phase() {
    let expected = expect_error("SyntaxError", Phase::CompileTime, "UndefinedVariable");

    assert_eq!(
        check_outcome(&expected, &raised("SyntaxError", "UndefinedVariable")),
        Ok(())
    );
    assert!(check_outcome(&expected, &raised("TypeError", "UndefinedVariable")).is_err());
    assert!(check_outcome(&expected, &raised("SyntaxError", "InvalidArgumentType")).is_err());
    let runtime = expect_error("SyntaxError", Phase::Runtime, "UndefinedVariable");
    assert_eq!(
        check_outcome(&runtime, &raised("SyntaxError", "UndefinedVariable")),
        Ok(())
    );
}

#[test]
fn wildcard_kind_and_detail_match_anything() {
    let any_kind = expect_error("Error", Phase::AnyTime, "UndefinedVariable");
    let any_detail = expect_error("TypeError", Phase::Runtime, "*");

    assert_eq!(
        check_outcome(&any_kind, &raised("SyntaxError", "UndefinedVariable")),
        Ok(())
    );
    assert!(check_outcome(&any_kind, &raised("SyntaxError", "Other")).is_err());
    assert_eq!(
        check_outcome(&any_detail, &raised("TypeError", "Whatever")),
        Ok(())
    );
    assert!(check_outcome(&any_detail, &raised("SyntaxError", "Whatever")).is_err());
}

#[test]
fn rows_where_an_error_is_expected_fail() {
    let expected = expect_error("SyntaxError", Phase::CompileTime, "*");

    assert!(check_outcome(&expected, &Ok(Rows::default())).is_err());
}

#[test]
fn an_error_where_rows_are_expected_fails() {
    let expected = expect_rows(&["n"], vec![], Ordering::AnyOrder, Lists::Strict);

    assert!(check_outcome(&expected, &raised("SyntaxError", "x")).is_err());
    assert!(check_outcome(&Expected::Empty, &raised("SyntaxError", "x")).is_err());
}

#[test]
fn an_unsupported_query_fails_even_when_an_error_is_expected() {
    let expected = expect_error("Error", Phase::AnyTime, "*");

    assert!(check_outcome(&expected, &Err(QueryError::Unsupported)).is_err());
}

#[test]
fn wildcards_raised_by_the_engine_match_only_themselves() {
    let expected = expect_error("SyntaxError", Phase::CompileTime, "UndefinedVariable");

    assert!(check_outcome(&expected, &raised("Error", "UndefinedVariable")).is_err());
    assert!(check_outcome(&expected, &raised("SyntaxError", "*")).is_err());
}
