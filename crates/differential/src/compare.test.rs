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
        Outcome::LadybugRejects {
            error: "Error: no".into(),
            rhizo: digest(&ok.clone().unwrap(), Ordering::AnyOrder, Lists::Strict),
        }
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
    let outcome = Outcome::LadybugRejects {
        error: "Parser exception:\n\tline 1".into(),
        rhizo: String::new(),
    };

    assert_eq!(outcome.detail(), "Parser exception: line 1");
}

#[test]
fn the_digest_follows_the_comparison_rules() {
    let digest_of = |rows: Vec<Vec<Value>>, ordering, lists| {
        digest(
            &Rows {
                columns: vec!["a".into()],
                rows,
            },
            ordering,
            lists,
        )
    };
    let list = |values: &[i64]| {
        vec![vec![Value::List(
            values.iter().map(|&v| Value::Integer(v)).collect(),
        )]]
    };

    assert_eq!(
        digest_of(ints(&[1, 2]), Ordering::AnyOrder, Lists::Strict),
        digest_of(ints(&[2, 1]), Ordering::AnyOrder, Lists::Strict)
    );
    assert_ne!(
        digest_of(ints(&[1, 2]), Ordering::InOrder, Lists::Strict),
        digest_of(ints(&[2, 1]), Ordering::InOrder, Lists::Strict)
    );
    assert_ne!(
        digest_of(ints(&[3]), Ordering::AnyOrder, Lists::Strict),
        digest_of(ints(&[2]), Ordering::AnyOrder, Lists::Strict)
    );
    assert_eq!(
        digest_of(list(&[1, 2]), Ordering::AnyOrder, Lists::IgnoreElementOrder),
        digest_of(list(&[2, 1]), Ordering::AnyOrder, Lists::IgnoreElementOrder)
    );
    assert_ne!(
        digest_of(list(&[1, 2]), Ordering::AnyOrder, Lists::Strict),
        digest_of(list(&[2, 1]), Ordering::AnyOrder, Lists::Strict)
    );
    assert_eq!(
        digest_of(
            vec![vec![Value::Float(0.1 + 0.2)]],
            Ordering::AnyOrder,
            Lists::Strict
        ),
        digest_of(
            vec![vec![Value::Float(0.3)]],
            Ordering::AnyOrder,
            Lists::Strict
        )
    );
}
