use std::collections::BTreeSet;

use tck::feature::Case;
use tck::scenario::Expected;
use tck::scenario::GraphSetup;
use tck::scenario::Ordering;
use tck::value::Lists;

use crate::random::Random;

/// The graph a query runs on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fixture {
    /// The generated graph, see [`crate::graph::Graph`].
    Generated,
    /// A graph with nothing in it.
    Empty,
}

/// One query of the corpus and how its results are compared.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Query {
    /// Stable, unique name, used in the expectations file.
    pub name: String,
    /// Query text, run unchanged on both engines.
    pub text: String,
    /// The graph it runs on.
    pub fixture: Fixture,
    /// Whether the query fixes the order of its rows.
    pub ordering: Ordering,
    /// Whether the order of list elements is meaningful.
    pub lists: Lists,
}

const UPDATING_OR_READING_WORDS: [&str; 10] = [
    "MATCH", "CREATE", "MERGE", "DELETE", "DETACH", "SET", "REMOVE", "CALL", "INSERT", "FOREACH",
];

/// Queries from the vendored TCK that need no graph: scenarios RhizoDB passes that start from an
/// empty graph with no setup, parameters, or procedures, expect rows, and neither read nor
/// update the graph.
///
/// LadybugDB needs a schema before it holds data, so scenarios that build or read a graph would
/// need one derived per scenario. Graph-free scenarios run on both engines unchanged.
pub fn from_tck(cases: &[Case], passing: &BTreeSet<String>) -> Vec<Query> {
    cases
        .iter()
        .filter(|case| passing.contains(&case.name))
        .filter_map(|case| {
            let scenario = case.scenario.as_ref().ok()?;
            let Expected::Rows {
                ordering, lists, ..
            } = &scenario.expected
            else {
                return None;
            };
            let graph_free = matches!(scenario.graph, GraphSetup::Empty | GraphSetup::Any)
                && scenario.setup.is_empty()
                && scenario.parameters.is_empty()
                && scenario.procedures.is_empty()
                && scenario.control.is_none()
                && !touches_the_graph(&scenario.query);
            graph_free.then(|| Query {
                name: format!("tck: {}", case.name),
                text: scenario.query.clone(),
                fixture: Fixture::Empty,
                ordering: *ordering,
                lists: *lists,
            })
        })
        .collect()
}

fn touches_the_graph(query: &str) -> bool {
    query
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .any(|word| {
            UPDATING_OR_READING_WORDS
                .iter()
                .any(|keyword| word.eq_ignore_ascii_case(keyword))
        })
}

/// Queries from a hand-written corpus file: one query per line, run on the generated graph.
/// Blank lines and lines starting with `#` are skipped. A query whose last `RETURN` has an
/// `ORDER BY` is compared in order, and a query that calls `collect` compares lists without
/// element order, since the order `collect` gathers rows in is not fixed.
pub fn hand_written(text: &str) -> Vec<Query> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| Query {
            name: format!("corpus: {line}"),
            text: line.to_string(),
            fixture: Fixture::Generated,
            ordering: if final_return_is_ordered(line) {
                Ordering::InOrder
            } else {
                Ordering::AnyOrder
            },
            lists: if line.contains("collect(") {
                Lists::IgnoreElementOrder
            } else {
                Lists::Strict
            },
        })
        .collect()
}

fn final_return_is_ordered(query: &str) -> bool {
    let upper = query.to_ascii_uppercase();
    upper
        .rfind("RETURN")
        .is_some_and(|start| upper[start..].contains("ORDER BY"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Var {
    P,
    Q,
    K,
    C,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Column {
    Scalar,
    Aggregate,
    Element,
    Collected,
}

const PATTERNS: [(&str, &[Var]); 9] = [
    ("MATCH (p:Person)", &[Var::P]),
    (
        "MATCH (p:Person)-[k:KNOWS]->(q:Person)",
        &[Var::P, Var::K, Var::Q],
    ),
    (
        "MATCH (p:Person)<-[k:KNOWS]-(q:Person)",
        &[Var::P, Var::K, Var::Q],
    ),
    (
        "MATCH (p:Person)-[k:KNOWS]-(q:Person)",
        &[Var::P, Var::K, Var::Q],
    ),
    ("MATCH (p:Person)-[:LIVES_IN]->(c:City)", &[Var::P, Var::C]),
    (
        "MATCH (p:Person)-[k:KNOWS]->(q:Person)-[:LIVES_IN]->(c:City)",
        &[Var::P, Var::K, Var::Q, Var::C],
    ),
    (
        "MATCH (p:Person) OPTIONAL MATCH (p)-[:LIVES_IN]->(c:City)",
        &[Var::P, Var::C],
    ),
    (
        "MATCH (p:Person)-[:KNOWS]->(:Person)-[:KNOWS]->(q:Person)",
        &[Var::P, Var::Q],
    ),
    ("MATCH (p:Person), (c:City)", &[Var::P, Var::C]),
];

type Template = fn(&mut Random) -> String;

const PREDICATES: [(&[Var], Template); 18] = [
    (&[Var::P], |r| format!("p.age > {}", r.between(18, 80))),
    (&[Var::P], |_| "p.age IS NULL".into()),
    (&[Var::P], |_| "p.age IS NOT NULL".into()),
    (&[Var::P], |r| {
        let names = ["Ada", "Alan", "Grace", "Nobody"];
        format!("p.name = '{}'", r.pick(&names).copied().unwrap_or_default())
    }),
    (&[Var::P], |_| "p.name STARTS WITH 'A'".into()),
    (&[Var::P], |_| "p.name CONTAINS 'a'".into()),
    (&[Var::P], |_| "p.age % 2 = 0".into()),
    (&[Var::P], |r| format!("p.id <= {}", r.between(1, 30))),
    (&[Var::P], |r| format!("NOT p.age < {}", r.between(18, 80))),
    (&[Var::P], |_| "p.age IN [20, 30, 40, 50, 60]".into()),
    (&[Var::P], |r| {
        let low = r.between(18, 60);
        format!("p.age >= {low} AND p.age <= {}", low + r.between(0, 20))
    }),
    (&[Var::Q], |r| format!("q.age < {}", r.between(18, 80))),
    (&[Var::P, Var::Q], |_| "q.id <> p.id".into()),
    (&[Var::P, Var::Q], |_| "q.name = p.name".into()),
    (&[Var::K], |r| {
        format!("k.since >= {}", r.between(1990, 2024))
    }),
    (&[Var::C], |r| {
        format!("c.population > {}", r.between(1, 50) * 1000)
    }),
    (&[Var::C], |_| "c.name = 'Oslo'".into()),
    (&[Var::C], |_| "c.name IS NULL".into()),
];

const PROJECTIONS: [(&[Var], &str, Column); 32] = [
    (&[Var::P], "p.name", Column::Scalar),
    (&[Var::P], "p.id", Column::Scalar),
    (&[Var::P], "p.age", Column::Scalar),
    (&[Var::P], "p.age + 1", Column::Scalar),
    (&[Var::P], "p.age * 2 - p.id", Column::Scalar),
    (&[Var::P], "p.age / 7", Column::Scalar),
    (&[Var::P], "p.age > 40", Column::Scalar),
    (&[Var::P], "p.name + '!'", Column::Scalar),
    (&[Var::P], "coalesce(p.age, -1)", Column::Scalar),
    (&[Var::P], "size(p.name)", Column::Scalar),
    (&[Var::P], "toUpper(p.name)", Column::Scalar),
    (&[Var::P], "p.age IS NULL", Column::Scalar),
    (&[Var::P], "p", Column::Element),
    (&[Var::Q], "q.name", Column::Scalar),
    (&[Var::Q], "q.id", Column::Scalar),
    (&[Var::P, Var::Q], "p.age > q.age", Column::Scalar),
    (&[Var::K], "k.since", Column::Scalar),
    (&[Var::K], "k", Column::Element),
    (&[Var::C], "c.name", Column::Scalar),
    (&[Var::C], "c.population", Column::Scalar),
    (&[Var::C], "c", Column::Element),
    (&[], "count(*)", Column::Aggregate),
    (&[Var::P], "count(p.age)", Column::Aggregate),
    (&[Var::P], "count(DISTINCT p.name)", Column::Aggregate),
    (&[Var::P], "sum(p.age)", Column::Aggregate),
    (&[Var::P], "min(p.age)", Column::Aggregate),
    (&[Var::P], "max(p.name)", Column::Aggregate),
    (&[Var::P], "avg(p.age)", Column::Aggregate),
    (&[Var::C], "count(c)", Column::Aggregate),
    (&[Var::K], "max(k.since)", Column::Aggregate),
    (&[Var::P], "collect(p.id)", Column::Collected),
    (&[Var::C], "collect(DISTINCT c.name)", Column::Collected),
];

const ALIASES: [&str; 3] = ["x", "y", "z"];

/// Generates `count` queries over the generated graph from `seed`. Each joins a pattern, an
/// optional filter, and a projection drawn from fixed tables, so the same seed always gives the
/// same queries. A query is ordered only by every column, so its order is total.
pub fn generated(seed: u64, count: usize) -> Vec<Query> {
    let mut random = Random::new(seed);
    let mut seen = BTreeSet::new();
    let mut queries = Vec::new();
    for _ in 0..count.saturating_mul(4) {
        if queries.len() == count {
            break;
        }
        let Some(query) = generate_one(&mut random) else {
            continue;
        };
        if seen.insert(query.text.clone()) {
            queries.push(query);
        }
    }
    queries
}

fn generate_one(random: &mut Random) -> Option<Query> {
    let (pattern, vars) = *random.pick(&PATTERNS)?;
    let bound = |needs: &[Var]| needs.iter().all(|var| vars.contains(var));
    let predicates: Vec<Template> = PREDICATES
        .iter()
        .filter(|(needs, _)| bound(needs))
        .map(|(_, template)| *template)
        .collect();
    let projections: Vec<(&str, Column)> = PROJECTIONS
        .iter()
        .filter(|(needs, _, _)| bound(needs))
        .map(|(_, text, column)| (*text, *column))
        .collect();

    let mut text = pattern.to_string();
    let filters = random.below(3);
    let conditions: Vec<String> = (0..filters)
        .filter_map(|_| Some(random.pick(&predicates)?(random)))
        .collect();
    if !conditions.is_empty() {
        let joiner = if random.chance(1, 3) { " OR " } else { " AND " };
        text.push_str(" WHERE ");
        text.push_str(&conditions.join(joiner));
    }

    let width = 1 + random.below(ALIASES.len());
    let columns: Vec<(&str, Column)> = (0..width)
        .filter_map(|_| random.pick(&projections).copied())
        .collect();
    let has = |kind: Column| columns.iter().any(|(_, column)| *column == kind);
    let distinct = !has(Column::Aggregate) && !has(Column::Collected) && random.chance(1, 4);
    let items: Vec<String> = columns
        .iter()
        .zip(ALIASES)
        .map(|((expression, _), alias)| format!("{expression} AS {alias}"))
        .collect();
    text.push_str(if distinct {
        " RETURN DISTINCT "
    } else {
        " RETURN "
    });
    text.push_str(&items.join(", "));

    let orderable = !has(Column::Element) && !has(Column::Collected);
    let ordered = orderable && random.chance(1, 2);
    if ordered {
        let keys: Vec<String> = ALIASES
            .iter()
            .take(items.len())
            .map(|alias| {
                let direction = if random.chance(1, 2) { " DESC" } else { "" };
                format!("{alias}{direction}")
            })
            .collect();
        text.push_str(" ORDER BY ");
        text.push_str(&keys.join(", "));
        if random.chance(1, 2) {
            text.push_str(&format!(" SKIP {}", random.below(4)));
        }
        if random.chance(1, 2) {
            text.push_str(&format!(" LIMIT {}", 1 + random.below(5)));
        }
    }
    Some(Query {
        name: format!("generated: {text}"),
        text,
        fixture: Fixture::Generated,
        ordering: if ordered {
            Ordering::InOrder
        } else {
            Ordering::AnyOrder
        },
        lists: if has(Column::Collected) {
            Lists::IgnoreElementOrder
        } else {
            Lists::Strict
        },
    })
}

#[cfg(test)]
#[path = "corpus.test.rs"]
mod tests;
