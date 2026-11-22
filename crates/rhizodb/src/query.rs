mod aggregate;
mod bind;
mod builtin;
mod cache;
mod compare;
mod datum;
mod eval;
mod exec;
mod functions;
mod matcher;
mod plan;
mod regex;
mod seek;
mod shape;
mod taint;
mod temporals;
mod update;

use std::sync::Arc;

use crate::error::Error;
use crate::error::ErrorKind;
use crate::gql::parser::parse;
use crate::graph::Graph;
use crate::graph::GraphTx;
use crate::procedure::Procedure;
use crate::procedure::Procedures;
use crate::result::Parameters;
use crate::result::QueryResult;
use crate::storage::ReadTransaction;
use crate::storage::Storage;
use crate::storage::WriteTransaction;
use crate::temporal::Clocks;
use crate::temporal::DateTime;

pub(crate) use self::builtin::is_reserved;
use self::cache::PlanCache;
use self::datum::Datum;
use self::datum::from_parameter;
use self::plan::Plan;

/// The procedures that `CALL` reaches and the plans of the queries bound against them, which a
/// database shares with its transactions.
#[derive(Debug, Default)]
pub(crate) struct Statements {
    procedures: Procedures,
    plans: PlanCache,
}

impl Statements {
    /// Makes `procedure` callable by its name, replacing any procedure of that name, and forgets
    /// every bound plan.
    pub(crate) fn register(&mut self, procedure: Procedure) {
        self.procedures
            .insert(procedure.signature.name.clone(), Arc::new(procedure));
        self.plans.clear();
    }

    fn prepare(
        &self,
        query: &str,
        parameters: &Parameters,
    ) -> Result<(Arc<Plan>, Vec<Datum>), Error> {
        let plan = match self.plans.get(query) {
            Some(plan) => plan,
            None => {
                let plan = Arc::new(bind::bind(
                    &parse(query)?,
                    query,
                    parameters,
                    &self.procedures,
                )?);
                self.plans.insert(query, Arc::clone(&plan));
                plan
            }
        };
        let values = plan
            .parameters()
            .iter()
            .map(|name| {
                parameters
                    .get(name)
                    .map(from_parameter)
                    .ok_or_else(|| bind::missing_parameter(name))
            })
            .collect::<Result<_, Error>>()?;
        Ok((plan, values))
    }
}

/// Parses, binds, and runs one query in a transaction of its own. A query that updates commits
/// only when every clause succeeded.
pub(crate) fn execute<S: Storage>(
    graph: &Graph<S>,
    statements: &Statements,
    query: &str,
    parameters: &Parameters,
) -> Result<QueryResult, Error> {
    let (plan, values) = statements.prepare(query, parameters)?;
    match &*plan {
        Plan::ReadOnly(program) => exec::run_read(
            &graph.read()?,
            program,
            &values,
            &Clocks::for_own_transaction(),
        ),
        Plan::Updating(program) => {
            let mut tx = graph.write()?;
            let result =
                exec::run_write(&mut tx, program, &values, &Clocks::for_own_transaction())?;
            tx.commit()?;
            Ok(result)
        }
    }
}

/// Parses, binds, and runs one query in a read transaction, refusing a query that updates.
pub(crate) fn execute_read<T: ReadTransaction>(
    tx: &GraphTx<T>,
    began: &DateTime,
    statements: &Statements,
    query: &str,
    parameters: &Parameters,
) -> Result<QueryResult, Error> {
    let (plan, values) = statements.prepare(query, parameters)?;
    match &*plan {
        Plan::ReadOnly(program) => exec::run_read(tx, program, &values, &Clocks::within(began)),
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
    statements: &Statements,
    query: &str,
    parameters: &Parameters,
) -> Result<QueryResult, Error> {
    let (plan, values) = statements.prepare(query, parameters)?;
    match &*plan {
        Plan::ReadOnly(program) => exec::run_read(tx, program, &values, &Clocks::within(began)),
        Plan::Updating(program) => exec::run_write(tx, program, &values, &Clocks::within(began)),
    }
}
