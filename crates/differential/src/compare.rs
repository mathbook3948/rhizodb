use tck::engine::QueryError;
use tck::engine::Rows;
use tck::matching::check_outcome;
use tck::scenario::Expected;
use tck::scenario::Ordering;
use tck::value::Lists;

/// How the two engines fared on one query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Both returned the same rows.
    Agree,
    /// Both returned rows, and the rows differ.
    Differ(String),
    /// RhizoDB returned rows and LadybugDB raised an error.
    LadybugRejects(String),
    /// LadybugDB returned rows and RhizoDB raised an error.
    RhizoRejects(String),
    /// Both raised an error.
    BothReject,
}

impl Outcome {
    /// The name the expectations file uses for this kind of outcome.
    pub fn kind(&self) -> &'static str {
        match self {
            Outcome::Agree => "agree",
            Outcome::Differ(_) => "differ",
            Outcome::LadybugRejects(_) => "ladybug-rejects",
            Outcome::RhizoRejects(_) => "rhizo-rejects",
            Outcome::BothReject => "both-reject",
        }
    }

    /// What the outcome says beyond its kind, on one line.
    pub fn detail(&self) -> String {
        let detail = match self {
            Outcome::Agree | Outcome::BothReject => "",
            Outcome::Differ(detail)
            | Outcome::LadybugRejects(detail)
            | Outcome::RhizoRejects(detail) => detail,
        };
        detail.split_whitespace().collect::<Vec<_>>().join(" ")
    }
}

/// Compares what the engines returned for one query.
///
/// Column names are not compared, since the engines name unaliased columns differently. Rows are
/// a multiset unless `ordering` says the query fixes their order.
pub fn compare(
    rhizo: &Result<Rows, QueryError>,
    ladybug: &Result<Rows, QueryError>,
    ordering: Ordering,
    lists: Lists,
) -> Outcome {
    match (rhizo, ladybug) {
        (Err(_), Err(_)) => Outcome::BothReject,
        (Ok(_), Err(error)) => Outcome::LadybugRejects(error.to_string()),
        (Err(error), Ok(_)) => Outcome::RhizoRejects(error.to_string()),
        (Ok(rhizo), Ok(ladybug)) if rhizo.columns.len() != ladybug.columns.len() => {
            Outcome::Differ(format!(
                "RhizoDB returned {} columns and LadybugDB {}",
                rhizo.columns.len(),
                ladybug.columns.len()
            ))
        }
        (Ok(rhizo), Ok(ladybug)) => {
            let expected = Expected::Rows {
                columns: ladybug.columns.clone(),
                rows: rhizo.rows.clone(),
                ordering,
                lists,
            };
            match check_outcome(&expected, &Ok(ladybug.clone())) {
                Ok(()) => Outcome::Agree,
                Err(_) => Outcome::Differ(format!(
                    "RhizoDB {:?}, LadybugDB {:?}",
                    rhizo.rows, ladybug.rows
                )),
            }
        }
    }
}

#[cfg(test)]
#[path = "compare.test.rs"]
mod tests;
