use tck::engine::QueryError;
use tck::engine::Rows;
use tck::matching::check_outcome;
use tck::scenario::Expected;
use tck::scenario::Ordering;
use tck::value::Lists;
use tck::value::Node;
use tck::value::Properties;
use tck::value::Relationship;
use tck::value::Value;

/// How the two engines fared on one query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Both returned the same rows.
    Agree,
    /// Both returned rows, and the rows differ: both engines' rows, and the [`digest`] of
    /// RhizoDB's.
    Differ { detail: String, rhizo: String },
    /// RhizoDB returned rows and LadybugDB raised an error: the error, and the [`digest`] of
    /// RhizoDB's rows.
    LadybugRejects { error: String, rhizo: String },
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
            Outcome::Differ { .. } => "differ",
            Outcome::LadybugRejects { .. } => "ladybug-rejects",
            Outcome::RhizoRejects(_) => "rhizo-rejects",
            Outcome::BothReject => "both-reject",
        }
    }

    /// What the outcome says beyond its kind, on one line.
    pub fn detail(&self) -> String {
        let detail = match self {
            Outcome::Agree | Outcome::BothReject => "",
            Outcome::Differ { detail, .. }
            | Outcome::LadybugRejects { error: detail, .. }
            | Outcome::RhizoRejects(detail) => detail,
        };
        detail.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    /// The digest of RhizoDB's rows, for the outcomes where RhizoDB returned rows that no
    /// agreement checks.
    pub fn rhizo(&self) -> Option<&str> {
        match self {
            Outcome::Differ { rhizo, .. } | Outcome::LadybugRejects { rhizo, .. } => Some(rhizo),
            _ => None,
        }
    }
}

/// A short, stable digest of rows, equal for rows that compare equal under `ordering` and
/// `lists`, so a listed query notices when RhizoDB's answer changes.
///
/// Floats are rounded to 12 significant digits, since the order a parallel aggregate adds them
/// in moves the last bits.
pub fn digest(rows: &Rows, ordering: Ordering, lists: Lists) -> String {
    let mut rendered: Vec<String> = rows
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|value| canonical(value, lists))
                .collect::<Vec<_>>()
                .join(",")
        })
        .collect();
    if ordering == Ordering::AnyOrder {
        rendered.sort();
    }
    let text = format!("{}|{}", rows.columns.len(), rendered.join(";"));
    let hash = text.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    });
    format!("{hash:016x}")
}

fn canonical(value: &Value, lists: Lists) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Boolean(value) => value.to_string(),
        Value::Integer(value) => format!("i{value}"),
        Value::Float(value) if value.is_nan() => "fNaN".to_string(),
        Value::Float(value) if *value == 0.0 => "f0".to_string(),
        Value::Float(value) => format!("f{value:.11e}"),
        Value::String(value) => format!("{value:?}"),
        Value::List(values) => {
            let mut elements: Vec<String> =
                values.iter().map(|value| canonical(value, lists)).collect();
            if lists == Lists::IgnoreElementOrder {
                elements.sort();
            }
            format!("[{}]", elements.join(","))
        }
        Value::Map(properties) => properties_text(properties, lists),
        Value::Node(node) => node_text(node, lists),
        Value::Relationship(relationship) => relationship_text(relationship, lists),
        Value::Path(path) => {
            let mut text = node_text(&path.start, lists);
            for step in &path.steps {
                text.push_str(&format!(
                    "{:?}{}{}",
                    step.direction,
                    relationship_text(&step.relationship, lists),
                    node_text(&step.node, lists)
                ));
            }
            format!("<{text}>")
        }
    }
}

fn properties_text(properties: &Properties, lists: Lists) -> String {
    let entries: Vec<String> = properties
        .iter()
        .map(|(key, value)| format!("{key:?}:{}", canonical(value, lists)))
        .collect();
    format!("{{{}}}", entries.join(","))
}

fn node_text(node: &Node, lists: Lists) -> String {
    let labels: Vec<&str> = node.labels.iter().map(String::as_str).collect();
    format!(
        "({}{})",
        labels.join(":"),
        properties_text(&node.properties, lists)
    )
}

fn relationship_text(relationship: &Relationship, lists: Lists) -> String {
    format!(
        "[{:?}{}]",
        relationship.rel_type,
        properties_text(&relationship.properties, lists)
    )
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
        (Ok(rows), Err(error)) => Outcome::LadybugRejects {
            error: error.to_string(),
            rhizo: digest(rows, ordering, lists),
        },
        (Err(error), Ok(_)) => Outcome::RhizoRejects(error.to_string()),
        (Ok(rhizo), Ok(ladybug)) if rhizo.columns.len() != ladybug.columns.len() => {
            Outcome::Differ {
                detail: format!(
                    "RhizoDB returned {} columns and LadybugDB {}",
                    rhizo.columns.len(),
                    ladybug.columns.len()
                ),
                rhizo: digest(rhizo, ordering, lists),
            }
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
                Err(_) => Outcome::Differ {
                    detail: format!("RhizoDB {:?}, LadybugDB {:?}", rhizo.rows, ladybug.rows),
                    rhizo: digest(rhizo, ordering, lists),
                },
            }
        }
    }
}

#[cfg(test)]
#[path = "compare.test.rs"]
mod tests;
