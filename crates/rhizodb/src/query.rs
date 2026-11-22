mod aggregate;
mod bind;
mod builtin;
mod compare;
mod datum;
mod eval;
mod exec;
mod functions;
mod matcher;
mod plan;
mod regex;
mod shape;
mod temporals;
mod update;

use crate::error::Error;
use crate::error::ErrorKind;
use crate::gql::parser::parse;
use crate::graph::Graph;
use crate::graph::GraphTx;
use crate::procedure::Procedures;
use crate::result::Parameters;
use crate::result::QueryResult;
use crate::storage::ReadTransaction;
use crate::storage::Storage;
use crate::storage::WriteTransaction;
use crate::temporal::Clocks;
use crate::temporal::DateTime;

pub(crate) use self::builtin::is_reserved;
use self::plan::Plan;

/// Parses, binds, and runs one query in a transaction of its own. A query that updates commits
/// only when every clause succeeded.
pub(crate) fn execute<S: Storage>(
    graph: &Graph<S>,
    procedures: &Procedures,
    query: &str,
    parameters: &Parameters,
) -> Result<QueryResult, Error> {
    match prepare(procedures, query, parameters)? {
        Plan::ReadOnly(program) => {
            exec::run_read(&graph.read()?, &program, &Clocks::for_own_transaction())
        }
        Plan::Updating(program) => {
            let mut tx = graph.write()?;
            let result = exec::run_write(&mut tx, &program, &Clocks::for_own_transaction())?;
            tx.commit()?;
            Ok(result)
        }
    }
}

/// Parses, binds, and runs one query in a read transaction, refusing a query that updates.
pub(crate) fn execute_read<T: ReadTransaction>(
    tx: &GraphTx<T>,
    began: &DateTime,
    procedures: &Procedures,
    query: &str,
    parameters: &Parameters,
) -> Result<QueryResult, Error> {
    match prepare(procedures, query, parameters)? {
        Plan::ReadOnly(program) => exec::run_read(tx, &program, &Clocks::within(began)),
        Plan::Updating(_) => Err(Error::new(
            ErrorKind::ReadOnlyTransaction,
            "a read transaction cannot run a query that updates the graph",
        )),
    }
}

/// Parses, binds, and runs one query in a write transaction, which the caller commits. A failed
/// query may leave some of its writes in the transaction.
pub(crate) fn execute_write<W: WriteTransaction>(
    tx: &mut GraphTx<W>,
    began: &DateTime,
    procedures: &Procedures,
    query: &str,
    parameters: &Parameters,
) -> Result<QueryResult, Error> {
    match prepare(procedures, query, parameters)? {
        Plan::ReadOnly(program) => exec::run_read(tx, &program, &Clocks::within(began)),
        Plan::Updating(program) => exec::run_write(tx, &program, &Clocks::within(began)),
    }
}

fn prepare(procedures: &Procedures, query: &str, parameters: &Parameters) -> Result<Plan, Error> {
    bind::bind(&parse(query)?, query, parameters, procedures)
}
