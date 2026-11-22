use std::fmt;
use std::fs;
use std::io;
use std::path::Path;

use crate::data::Dataset;
use crate::fingerprint::Fingerprint;

/// The databases the benchmark measures.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EngineKind {
    Rhizodb,
    Ladybugdb,
}

impl EngineKind {
    /// The name rows and JSON carry.
    pub fn name(self) -> &'static str {
        match self {
            EngineKind::Rhizodb => "rhizodb",
            EngineKind::Ladybugdb => "ladybugdb",
        }
    }
}

/// The query languages the engines speak.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Dialect {
    Gql,
    Cypher,
    Sql,
    Datalog,
}

/// One query written in each dialect that has a form of it. A dialect without a form is
/// unsupported for that workload, which is a result, not a failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Texts {
    pub gql: Option<&'static str>,
    pub cypher: Option<&'static str>,
    pub sql: Option<&'static str>,
    pub datalog: Option<&'static str>,
}

impl Texts {
    /// A query that GQL and Cypher both read the same way.
    pub const fn graph(text: &'static str) -> Texts {
        Texts {
            gql: Some(text),
            cypher: Some(text),
            sql: None,
            datalog: None,
        }
    }

    /// The text in `dialect`, or an unsupported outcome when there is none.
    pub fn for_dialect(&self, dialect: Dialect) -> Result<&'static str, EngineError> {
        let text = match dialect {
            Dialect::Gql => self.gql,
            Dialect::Cypher => self.cypher,
            Dialect::Sql => self.sql,
            Dialect::Datalog => self.datalog,
        };
        text.ok_or_else(|| EngineError::Unsupported(format!("no {dialect:?} form of this query")))
    }
}

/// A query parameter in the types both engines take.
#[derive(Clone, Debug, PartialEq)]
pub enum Param {
    Int(i64),
    Text(String),
    Floats(Vec<f64>),
    NullInt,
    Rows(Vec<Vec<(&'static str, Param)>>),
}

/// A query text with the parameters it refers to as `$name`.
#[derive(Clone, Debug, PartialEq)]
pub struct Query<'a> {
    pub text: &'a str,
    pub params: Vec<(&'static str, Param)>,
}

impl<'a> Query<'a> {
    /// A query without parameters.
    pub fn new(text: &'a str) -> Self {
        Query {
            text,
            params: Vec::new(),
        }
    }

    /// Adds an integer parameter.
    pub fn int(mut self, name: &'static str, value: i64) -> Self {
        self.params.push((name, Param::Int(value)));
        self
    }

    /// Adds a string parameter.
    pub fn text(mut self, name: &'static str, value: &str) -> Self {
        self.params.push((name, Param::Text(value.to_owned())));
        self
    }

    /// Adds a list of floats parameter.
    pub fn floats(mut self, name: &'static str, value: &[f64]) -> Self {
        self.params.push((name, Param::Floats(value.to_vec())));
        self
    }

    /// Adds a list of rows parameter, for `UNWIND $name AS row`.
    pub fn rows(mut self, name: &'static str, rows: Vec<Vec<(&'static str, Param)>>) -> Self {
        self.params.push((name, Param::Rows(rows)));
        self
    }
}

/// Why an engine did not do what a workload asked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EngineError {
    /// The operation ran and failed.
    Failed(String),
    /// The engine cannot do the operation here, which is not a failure of the measurement.
    Unsupported(String),
}

impl fmt::Display for EngineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EngineError::Failed(message) => write!(formatter, "{message}"),
            EngineError::Unsupported(message) => write!(formatter, "unsupported: {message}"),
        }
    }
}

impl std::error::Error for EngineError {}

/// One connection to an open database. A session runs one query at a time.
pub trait Session {
    /// Parses and plans `text` ahead of time where the engine can, so a later run skips that.
    fn prepare(&mut self, text: &str) -> Result<(), EngineError>;

    /// Runs one query, walks every row it returns, and fingerprints them.
    fn run(&mut self, query: &Query<'_>) -> Result<Fingerprint, EngineError>;

    /// Starts a write transaction that every later query joins until [`Session::commit`].
    fn begin(&mut self) -> Result<(), EngineError>;

    /// Commits the transaction [`Session::begin`] started.
    fn commit(&mut self) -> Result<(), EngineError>;
}

/// An open database. Adding an engine means one module that implements [`Engine`], [`Instance`],
/// and [`Session`].
pub trait Instance: Sync {
    /// Opens a session. Each thread opens its own.
    fn session(&self) -> Result<Box<dyn Session + '_>, EngineError>;

    /// Declares what the engine needs before data loads, such as RhizoDB's property indexes on the
    /// ids that LadybugDB keys its tables by.
    fn create_schema(&self) -> Result<(), EngineError>;

    /// Writes the input files the engine's own bulk loader reads, outside any timing. Does
    /// nothing when the files exist.
    fn prepare_bulk_load(&self, data: &Dataset, scratch: &Path) -> Result<(), EngineError>;

    /// Closes the database and returns how many bytes it occupies on disk, or `None` when the
    /// engine keeps its data where this process cannot measure it. The default closes it and
    /// adds up the files under `dir`.
    fn finish(self: Box<Self>, dir: &Path) -> Result<Option<u64>, EngineError> {
        drop(self);
        directory_size(dir)
            .map(Some)
            .map_err(|error| EngineError::Failed(error.to_string()))
    }

    /// Loads the whole dataset the way the engine loads fastest.
    fn bulk_load(&self, data: &Dataset, scratch: &Path) -> Result<(), EngineError>;

    /// Creates the keyword index over the person `bio`.
    fn create_keyword_index(&self) -> Result<(), EngineError>;

    /// Creates the vector index over the person `embedding`.
    fn create_vector_index(&self) -> Result<(), EngineError>;

    /// The query that searches the keyword index for `$term` and returns person ids.
    fn keyword_search(&self) -> &'static str;

    /// The query that searches the vector index for `$vector` and returns person ids.
    fn vector_search(&self) -> &'static str;
}

/// A database engine under test.
pub trait Engine: Sync {
    /// Which engine this is.
    fn kind(&self) -> EngineKind;

    /// The language the engine's queries are written in.
    fn dialect(&self) -> Dialect;

    /// Opens the database in `dir`, creating it when `dir` holds none.
    fn open(&self, dir: &Path) -> Result<Box<dyn Instance>, EngineError>;
}

/// The bytes of every file under `dir`.
pub fn directory_size(dir: &Path) -> io::Result<u64> {
    let mut total = 0;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        total += if metadata.is_dir() {
            directory_size(&entry.path())?
        } else {
            metadata.len()
        };
    }
    Ok(total)
}

#[cfg(test)]
#[path = "engine.test.rs"]
mod tests;
