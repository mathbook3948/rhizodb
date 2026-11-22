mod load;
mod read;
mod search;
mod write;

use std::collections::BTreeSet;
use std::fmt;
use std::io;
use std::path::Path;

use crate::data::Dataset;
use crate::engine::Engine;
use crate::engine::EngineError;
use crate::engine::Instance;
use crate::engine::Query;
use crate::engine::Texts;
use crate::measure::Measurement;
use crate::measure::Tally;
use crate::measure::timed;
use crate::scale::Plan;

/// The database state a workload starts from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Needs {
    /// The loaded graph, opened once and shared by every read workload.
    Base,
    /// The loaded graph with the keyword and vector indexes, opened once and shared.
    Indexed,
    /// A fresh copy of the loaded graph, opened for the run and removed after it.
    Copy,
    /// A fresh copy of the loaded graph that the workload opens itself.
    ClosedCopy,
    /// An empty directory that the workload opens itself.
    Empty,
}

/// How the fingerprints of the two engines are compared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Comparison {
    /// Row count and checksum must match.
    Exact,
    /// Only the row count must match, for results the engines rank differently.
    RowsOnly,
}

/// Whether each index the search workloads need was created on the shared indexed graph.
#[derive(Debug)]
pub struct IndexState {
    pub keyword: Result<(), EngineError>,
    pub vector: Result<(), EngineError>,
}

/// What a workload gets to work with.
pub struct Context<'a> {
    pub engine: &'a dyn Engine,
    pub instance: Option<&'a dyn Instance>,
    pub indexes: Option<&'a IndexState>,
    pub data: &'a Dataset,
    pub plan: &'a Plan,
    pub dir: &'a Path,
    pub scratch: &'a Path,
}

impl fmt::Debug for Context<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Context")
            .field("engine", &self.engine.kind())
            .field("dir", &self.dir)
            .finish_non_exhaustive()
    }
}

impl Context<'_> {
    pub(crate) fn text(&self, texts: &Texts) -> Result<&'static str, EngineError> {
        texts.for_dialect(self.engine.dialect())
    }

    pub(crate) fn instance(&self) -> Result<&dyn Instance, EngineError> {
        self.instance
            .ok_or_else(|| EngineError::Failed("the workload has no open database".to_owned()))
    }
}

/// The function that runs a workload once and returns what it measured.
pub type Run = fn(&Context<'_>) -> Result<Measurement, EngineError>;

/// One measured operation with the state it starts from.
#[derive(Clone, Copy, Debug)]
pub struct Workload {
    pub name: &'static str,
    pub needs: Needs,
    pub comparison: Comparison,
    pub run: Run,
}

/// Every workload, in the order they run.
pub fn all() -> Vec<Workload> {
    let mut workloads = Vec::new();
    workloads.extend(load::workloads());
    workloads.extend(write::small_commits());
    workloads.extend(read::workloads());
    workloads.extend(write::bulk());
    workloads.extend(search::workloads());
    workloads
}

pub(crate) fn run_queries(
    ctx: &Context<'_>,
    queries: &[Query<'_>],
) -> Result<Measurement, EngineError> {
    let instance = ctx.instance()?;
    let mut session = instance.session()?;
    let texts: BTreeSet<&str> = queries.iter().map(|query| query.text).collect();
    for text in texts {
        session.prepare(text)?;
    }
    let mut tally = Tally::default();
    let (elapsed, ()) = timed(|| {
        for query in queries {
            tally.record(session.run(query));
        }
    });
    Ok(tally.finish(elapsed))
}

pub(crate) fn io_failure(error: io::Error) -> EngineError {
    EngineError::Failed(error.to_string())
}
