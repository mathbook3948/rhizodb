use std::collections::BTreeMap;

use lbug::Connection;
use lbug::Database;
use lbug::SystemConfig;
use tck::engine::Engine;
use tck::rhizo::RhizoDb;

use crate::compare::Outcome;
use crate::compare::compare;
use crate::corpus::Fixture;
use crate::corpus::Query;
use crate::graph::Graph;
use crate::graph::LADYBUG_SCHEMA;
use crate::ladybug;

/// Runs every query on both engines and compares the results, in the order given.
///
/// Each engine holds two graphs: the generated one, loaded once, and an empty one. Queries only
/// read, so they share them. Fails only when a graph cannot be built.
pub fn run_all(queries: &[Query], graph: &Graph) -> Result<Vec<Outcome>, String> {
    let statements = graph.statements();
    let mut rhizo_generated = RhizoDb.fresh_graph();
    let mut rhizo_empty = RhizoDb.fresh_graph();
    for statement in &statements {
        rhizo_generated
            .execute(statement, &BTreeMap::new())
            .map_err(|error| format!("RhizoDB cannot load `{statement}`: {error}"))?;
    }

    let generated_database = open_ladybug()?;
    let empty_database = open_ladybug()?;
    let ladybug_generated = connect(&generated_database)?;
    let ladybug_empty = connect(&empty_database)?;
    for statement in LADYBUG_SCHEMA
        .iter()
        .copied()
        .chain(statements.iter().map(String::as_str))
    {
        ladybug::execute(&ladybug_generated, statement)
            .map_err(|error| format!("LadybugDB cannot load `{statement}`: {error}"))?;
    }

    Ok(queries
        .iter()
        .map(|query| {
            let (rhizo, ladybug) = match query.fixture {
                Fixture::Generated => (&mut rhizo_generated, &ladybug_generated),
                Fixture::Empty => (&mut rhizo_empty, &ladybug_empty),
            };
            compare(
                &rhizo.execute(&query.text, &BTreeMap::new()),
                &ladybug::execute(ladybug, &query.text),
                query.ordering,
                query.lists,
            )
        })
        .collect())
}

fn open_ladybug() -> Result<Database, String> {
    Database::in_memory(SystemConfig::default())
        .map_err(|error| format!("cannot open LadybugDB: {error}"))
}

fn connect(database: &Database) -> Result<Connection<'_>, String> {
    Connection::new(database).map_err(|error| format!("cannot connect to LadybugDB: {error}"))
}
