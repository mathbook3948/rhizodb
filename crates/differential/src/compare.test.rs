use tck::value::Value;

use super::*;

fn rows(columns: &[&str], rows: Vec<Vec<Value>>) -> Result<Rows, QueryError> {
    Ok(Rows {
        columns: columns.iter().map(|column| column.to_string()).collect(),
        rows,
    })
}

fn raised(detail: &str) -> Result<Rows, QueryError> {
    Err(QueryError::Raised {
        kind: "Error".into(),
        detail: detail.into(),
    })
}

fn ints(values: &[i64]) -> Vec<Vec<Value>> {
    values
        .iter()
        .map(|&value| vec![Value::Integer(value)])
        .collect()
}

#[test]
fn rows_in_another_order_agree_unless_the_order_is_fixed() {
    let rhizo = rows(&["a"], ints(&[1, 2]));
    let ladybug = rows(&["a"], ints(&[2, 1]));

    assert_eq!(
        compare(&rhizo, &ladybug, Ordering::AnyOrder, Lists::Strict),
        Outcome::Agree
    );
    assert_eq!(
        compare(&rhizo, &ladybug, Ordering::InOrder, Lists::Strict).kind(),
        "differ"
    );
}

#[test]
fn column_names_are_not_compared_but_their_number_is() {
    let rhizo = rows(&["1 + 2"], ints(&[3]));

    assert_eq!(
        compare(
            &rhizo,
            &rows(&["+(1,2)"], ints(&[3])),
            Ordering::AnyOrder,
            Lists::Strict
        ),
        Outcome::Agree
    );
    assert_eq!(
        compare(
            &rhizo,
            &rows(&["a", "b"], vec![vec![Value::Integer(3), Value::Null]]),
            Ordering::AnyOrder,
            Lists::Strict
        )
        .kind(),
        "differ"
    );
}

#[test]
fn a_missing_or_extra_row_differs() {
    assert_eq!(
        compare(
            &rows(&["a"], ints(&[1, 1])),
            &rows(&["a"], ints(&[1])),
            Ordering::AnyOrder,
            Lists::Strict
        )
        .kind(),
        "differ"
    );
}

#[test]
fn list_element_order_is_ignored_only_when_asked() {
    let rhizo = rows(
        &["l"],
        vec![vec![Value::List(vec![
            Value::Integer(1),
            Value::Integer(2),
        ])]],
    );
    let ladybug = rows(
        &["l"],
        vec![vec![Value::List(vec![
            Value::Integer(2),
            Value::Integer(1),
        ])]],
    );

    assert_eq!(
        compare(
            &rhizo,
            &ladybug,
            Ordering::AnyOrder,
            Lists::IgnoreElementOrder
        ),
        Outcome::Agree
    );
    assert_eq!(
        compare(&rhizo, &ladybug, Ordering::AnyOrder, Lists::Strict).kind(),
        "differ"
    );
}

#[test]
fn errors_are_classified_by_which_engine_raised_them() {
    let ok = rows(&["a"], ints(&[1]));

    assert_eq!(
        compare(&ok, &raised("no"), Ordering::AnyOrder, Lists::Strict),
        Outcome::LadybugRejects("Error: no".into())
    );
    assert_eq!(
        compare(&raised("no"), &ok, Ordering::AnyOrder, Lists::Strict),
        Outcome::RhizoRejects("Error: no".into())
    );
    assert_eq!(
        compare(
            &raised("a"),
            &raised("b"),
            Ordering::AnyOrder,
            Lists::Strict
        ),
        Outcome::BothReject
    );
}

#[test]
fn detail_fits_on_one_line() {
    let outcome = Outcome::LadybugRejects("Parser exception:\n\tline 1".into());

    assert_eq!(outcome.detail(), "Parser exception: line 1");
}
