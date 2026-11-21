mod aggregate;
mod bind;
mod compare;
mod datum;
mod eval;
mod exec;
mod functions;
mod matcher;
mod plan;
mod update;

use crate::error::Error;
use crate::gql::parser::parse;
use crate::graph::Graph;
use crate::result::Parameters;
use crate::result::QueryResult;
use crate::storage::Storage;

use self::plan::Plan;

/// Parses, binds, and runs one query in one transaction. A query that updates commits only when
/// every clause succeeded.
pub(crate) fn execute<S: Storage>(
    graph: &Graph<S>,
    query: &str,
    parameters: &Parameters,
) -> Result<QueryResult, Error> {
    let syntax = parse(query)?;
    match bind::bind(&syntax, query, parameters)? {
        Plan::ReadOnly(program) => exec::run_read(&graph.read()?, &program),
        Plan::Updating(program) => {
            let mut tx = graph.write()?;
            let result = exec::run_write(&mut tx, &program)?;
            tx.commit()?;
            Ok(result)
        }
    }
}
