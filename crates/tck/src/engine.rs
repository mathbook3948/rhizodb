use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use crate::scenario::Procedure;
use crate::side_effects::GraphSnapshot;
use crate::value::Value;

/// A result table. Each row is aligned with `columns`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Rows {
    /// Column names in order.
    pub columns: Vec<String>,
    /// Row values.
    pub rows: Vec<Vec<Value>>,
}

/// Why a query did not produce rows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QueryError {
    /// The engine cannot run the query at all.
    Unsupported,
    /// The engine raised a TCK error, with a `kind` such as `SyntaxError` and a `detail` code such
    /// as `UndefinedVariable`.
    Raised { kind: String, detail: String },
}

impl fmt::Display for QueryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QueryError::Unsupported => write!(formatter, "unsupported"),
            QueryError::Raised { kind, detail } => write!(formatter, "{kind}: {detail}"),
        }
    }
}

impl Error for QueryError {}

/// One graph the runner drives. A scenario gets its own graph from [`Engine::fresh_graph`].
pub trait Graph {
    /// Makes a procedure callable by later queries.
    fn register_procedure(&mut self, procedure: &Procedure);

    /// Runs a query and returns its rows, or the error it raised.
    fn execute(
        &mut self,
        query: &str,
        parameters: &BTreeMap<String, Value>,
    ) -> Result<Rows, QueryError>;

    /// Reports the observable state of the graph so the runner can diff side effects, or why the
    /// state could not be read.
    fn snapshot(&self) -> Result<GraphSnapshot, String>;
}

/// A query engine under test.
pub trait Engine: Send + Sync {
    /// Creates an empty graph that no other scenario shares.
    fn fresh_graph(&self) -> Box<dyn Graph>;
}

/// An engine that supports nothing, so every scenario fails at the engine.
#[derive(Clone, Copy, Debug, Default)]
pub struct Unimplemented;

impl Engine for Unimplemented {
    fn fresh_graph(&self) -> Box<dyn Graph> {
        Box::new(UnimplementedGraph)
    }
}

#[derive(Debug)]
struct UnimplementedGraph;

impl Graph for UnimplementedGraph {
    fn register_procedure(&mut self, _procedure: &Procedure) {}

    fn execute(
        &mut self,
        _query: &str,
        _parameters: &BTreeMap<String, Value>,
    ) -> Result<Rows, QueryError> {
        Err(QueryError::Unsupported)
    }

    fn snapshot(&self) -> Result<GraphSnapshot, String> {
        Ok(GraphSnapshot::default())
    }
}
