use crate::engine::QueryError;
use crate::engine::Rows;
use crate::scenario::Expected;
use crate::scenario::Ordering;
use crate::value::Lists;
use crate::value::Value;

const ANY_ERROR_KIND: &str = "Error";
const ANY_ERROR_DETAIL: &str = "*";

/// Checks a query result against what the scenario expects.
///
/// Error matching compares kind and detail and ignores the phase. An expected kind `Error` or an
/// expected detail `*` matches anything. The engine cannot use either as a wildcard.
pub fn check_outcome(expected: &Expected, actual: &Result<Rows, QueryError>) -> Result<(), String> {
    match (expected, actual) {
        (_, Err(QueryError::Unsupported)) => Err("the engine does not support the query".into()),
        (Expected::Error { kind, detail, .. }, Err(QueryError::Raised { kind: k, detail: d })) => {
            let kind_matches = kind == k || kind == ANY_ERROR_KIND;
            let detail_matches = detail == d || detail == ANY_ERROR_DETAIL;
            if kind_matches && detail_matches {
                Ok(())
            } else {
                Err(format!("expected error {kind}: {detail}, got {k}: {d}"))
            }
        }
        (Expected::Error { kind, detail, .. }, Ok(rows)) => Err(format!(
            "expected error {kind}: {detail}, got {} rows",
            rows.rows.len()
        )),
        (_, Err(error)) => Err(format!("expected a result, got error {error}")),
        (Expected::Empty, Ok(rows)) if rows.columns.is_empty() && rows.rows.is_empty() => Ok(()),
        (Expected::Empty, Ok(rows)) => Err(format!("expected an empty result, got {rows:?}")),
        (
            Expected::Rows {
                columns,
                rows,
                ordering,
                lists,
            },
            Ok(actual),
        ) => {
            if rows_match(columns, rows, *ordering, *lists, actual) {
                Ok(())
            } else {
                Err(format!(
                    "expected ({ordering:?}) {columns:?} {rows:?}, got {actual:?}"
                ))
            }
        }
    }
}

fn rows_match(
    columns: &[String],
    expected: &[Vec<Value>],
    ordering: Ordering,
    lists: Lists,
    actual: &Rows,
) -> bool {
    if columns != actual.columns || expected.len() != actual.rows.len() {
        return false;
    }
    match ordering {
        Ordering::InOrder => expected
            .iter()
            .zip(&actual.rows)
            .all(|(want, got)| row_equals(want, got, lists)),
        Ordering::AnyOrder => {
            let mut unmatched: Vec<&Vec<Value>> = actual.rows.iter().collect();
            expected.iter().all(|want| {
                match unmatched
                    .iter()
                    .position(|got| row_equals(want, got, lists))
                {
                    Some(index) => {
                        unmatched.swap_remove(index);
                        true
                    }
                    None => false,
                }
            })
        }
    }
}

fn row_equals(want: &[Value], got: &[Value], lists: Lists) -> bool {
    want.len() == got.len() && want.iter().zip(got).all(|(a, b)| a.equals(b, lists))
}

#[cfg(test)]
#[path = "matching.test.rs"]
mod tests;
