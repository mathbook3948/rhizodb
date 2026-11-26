mod aggregate;
mod bind;
mod builtin;
mod cache;
mod compare;
mod datum;
mod eval;
mod exec;
mod functions;
mod keys;
mod matcher;
mod parallel;
mod plan;
mod prune;
mod regex;
mod scalars;
mod scan;
mod seek;
mod shape;
mod taint;
mod temporals;
mod types;
mod update;

use std::sync::Arc;
use std::sync::PoisonError;
use std::sync::RwLock;

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
pub(crate) use self::eval::float_text;
pub(crate) use self::parallel::Parallelism;
use self::parallel::Workers;
use self::plan::Plan;

/// The procedures that `CALL` reaches, the plans of the queries bound against them, and how many
/// threads a read query may run on, which a database shares with its transactions. A plan is looked up, or bound and cached, while the
/// procedures are locked for reading, so a registration, which locks them for writing, never
/// leaves a plan bound against the procedures it replaced.
#[derive(Debug, Default)]
pub(crate) struct Statements {
    procedures: RwLock<Procedures>,
    plans: PlanCache,
    parallelism: Parallelism,
    workers: Workers,
}

impl Statements {
    pub(crate) fn new(parallelism: Parallelism) -> Self {
        Self {
            parallelism,
            ..Self::default()
        }
    }

    /// Makes `procedure` callable by its name, replacing any procedure of that name, and forgets
    /// every bound plan.
    pub(crate) fn register(&self, procedure: Procedure) {
        let mut procedures = self
            .procedures
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        procedures.insert(procedure.signature.name.clone(), Arc::new(procedure));
        self.plans.clear();
    }

    fn prepare(
        &self,
        query: &str,
        parameters: &Parameters,
    ) -> Result<(Arc<Plan>, Vec<Datum>), Error> {
        let procedures = self
            .procedures
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        let plan = match self.plans.get(query) {
            Some(plan) => plan,
            None => {
                let plan = Arc::new(bind::bind(&parse(query)?, query, parameters, &procedures)?);
                self.plans.insert(query, Arc::clone(&plan));
                plan
            }
        };
        drop(procedures);
        let values = plan
            .parameters()
            .iter()
            .map(|name| {
                parameters
                    .get(name)
                    .ok_or_else(|| bind::missing_parameter(name))
                    .and_then(from_parameter)
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
) -> Result<QueryResult, Error>
where
    for<'a> GraphTx<S::Read<'a>>: Sync,
{
    let (plan, values) = statements.prepare(query, parameters)?;
    match &*plan {
        Plan::ReadOnly(program) => parallel::run_read(
            &graph.read()?,
            program,
            &values,
            &Clocks::for_own_transaction(),
            statements.parallelism,
            &statements.workers,
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
) -> Result<QueryResult, Error>
where
    GraphTx<T>: Sync,
{
    let (plan, values) = statements.prepare(query, parameters)?;
    match &*plan {
        Plan::ReadOnly(program) => parallel::run_read(
            tx,
            program,
            &values,
            &Clocks::within(began),
            statements.parallelism,
            &statements.workers,
        ),
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
