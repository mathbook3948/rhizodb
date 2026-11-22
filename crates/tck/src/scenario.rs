use std::collections::BTreeMap;
use std::collections::BTreeSet;

use crate::side_effects::SideEffects;
use crate::value::Lists;
use crate::value::Value;

/// How the rows of an expected result are matched.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Ordering {
    /// Rows must come back in the listed order.
    InOrder,
    /// Rows are a multiset.
    AnyOrder,
}

/// The graph a scenario starts from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphSetup {
    /// A graph with no nodes.
    Empty,
    /// Any graph, since the scenario does not depend on the contents.
    Any,
    /// A named graph from `graphs/<name>/<name>.cypher`.
    Named(String),
}

/// A procedure the engine must expose, with the rows it returns.
#[derive(Clone, Debug, PartialEq)]
pub struct Procedure {
    /// Signature such as `test.my.proc(in :: INTEGER?) :: (out :: INTEGER?)`.
    pub signature: String,
    /// Column names of the table, inputs first.
    pub columns: Vec<String>,
    /// Rows of the table, aligned with `columns`.
    pub rows: Vec<Vec<Value>>,
}

/// What a scenario expects from a query.
#[derive(Clone, Debug, PartialEq)]
pub enum Expected {
    /// No columns and no rows.
    Empty,
    /// A table of rows. `columns` are the column names, `rows` align with them, `ordering` says
    /// whether row order matters, and `lists` whether list element order matters.
    Rows {
        columns: Vec<String>,
        rows: Vec<Vec<Value>>,
        ordering: Ordering,
        lists: Lists,
    },
    /// A raised error. `kind` is such as `SyntaxError` or `Error` for any kind, `phase` says when
    /// it is raised, and `detail` is a code such as `UndefinedVariable` or `*` for any detail.
    Error {
        kind: String,
        phase: Phase,
        detail: String,
    },
}

/// When an error is raised.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Phase {
    /// While the query is compiled.
    CompileTime,
    /// While the query runs.
    Runtime,
    /// Either time.
    AnyTime,
}

/// A query run after the main one, checked against its own expectation.
#[derive(Clone, Debug, PartialEq)]
pub struct Control {
    /// Query text.
    pub query: String,
    /// What the control query must return.
    pub expected: Expected,
}

/// One compiled TCK scenario, with outlines already expanded.
#[derive(Clone, Debug, PartialEq)]
pub struct Scenario {
    /// Feature file path relative to the features directory.
    pub feature: String,
    /// Scenario name as written in the feature file.
    pub name: String,
    /// Zero-based index of the example row for scenarios expanded from an outline.
    pub example: Option<usize>,
    /// Tags of the feature, scenario, and example table.
    pub tags: BTreeSet<String>,
    /// Starting graph.
    pub graph: GraphSetup,
    /// Procedures to register before the setup queries.
    pub procedures: Vec<Procedure>,
    /// Queries that build the starting state, run in order.
    pub setup: Vec<String>,
    /// Query parameters.
    pub parameters: BTreeMap<String, Value>,
    /// The query under test.
    pub query: String,
    /// Expected outcome of the query.
    pub expected: Expected,
    /// Expected side effects, or `None` when the scenario does not assert them.
    pub side_effects: Option<SideEffects>,
    /// An optional control query that runs after the checks above.
    pub control: Option<Control>,
}

impl Scenario {
    /// The stable, unique trial name of this scenario.
    pub fn trial_name(&self) -> String {
        trial_name(&self.feature, &self.name, self.example)
    }
}

/// Builds a trial name from its parts. Example indexes are shown one-based.
pub fn trial_name(feature: &str, name: &str, example: Option<usize>) -> String {
    match example {
        Some(index) => format!("{feature}: {name} #{}", index + 1),
        None => format!("{feature}: {name}"),
    }
}
