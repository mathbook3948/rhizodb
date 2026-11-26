use std::fmt;
use std::fs;
use std::io;
use std::path::Path;

use crate::data::Dataset;
use crate::fingerprint::Fingerprint;

/// The databases the benchmark measures.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum EngineKind {
    Rhizodb,
    Ladybugdb,
    Sqlite,
    Duckdb,
    Cozodb,
    Neo4j,
    Memgraph,
    Falkordb,
}

/// Whether an engine runs inside this process or is a separate server it connects to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
    Embedded,
    Server,
}

impl Category {
    /// The word the report uses.
    pub fn name(self) -> &'static str {
        match self {
            Category::Embedded => "embedded",
            Category::Server => "server",
        }
    }
}

/// Where an engine keeps the data it was loaded with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Storage {
    File,
    Memory,
}

impl Storage {
    /// The word the report uses.
    pub fn name(self) -> &'static str {
        match self {
            Storage::File => "file-backed",
            Storage::Memory => "in-memory",
        }
    }
}

impl EngineKind {
    /// Every engine, in the order reports list them.
    pub const ALL: [EngineKind; 8] = [
        EngineKind::Rhizodb,
        EngineKind::Ladybugdb,
        EngineKind::Sqlite,
        EngineKind::Duckdb,
        EngineKind::Cozodb,
        EngineKind::Neo4j,
        EngineKind::Memgraph,
        EngineKind::Falkordb,
    ];

    /// The name rows and JSON carry.
    pub fn name(self) -> &'static str {
        match self {
            EngineKind::Rhizodb => "rhizodb",
            EngineKind::Ladybugdb => "ladybugdb",
            EngineKind::Sqlite => "sqlite",
            EngineKind::Duckdb => "duckdb",
            EngineKind::Cozodb => "cozodb",
            EngineKind::Neo4j => "neo4j",
            EngineKind::Memgraph => "memgraph",
            EngineKind::Falkordb => "falkordb",
        }
    }

    /// Reads the name [`EngineKind::name`] gives.
    pub fn parse(text: &str) -> Option<EngineKind> {
        EngineKind::ALL.into_iter().find(|kind| kind.name() == text)
    }

    /// Embedded engines run in this process, servers are reached over a socket.
    pub fn category(self) -> Category {
        match self {
            EngineKind::Neo4j | EngineKind::Memgraph | EngineKind::Falkordb => Category::Server,
            _ => Category::Embedded,
        }
    }

    /// Whether the data lives in files this process can measure, or only in memory.
    pub fn storage(self) -> Storage {
        match self {
            EngineKind::Memgraph | EngineKind::Falkordb => Storage::Memory,
            _ => Storage::File,
        }
    }

    /// Whether the engine's keyword search must return exactly the persons whose bio holds the
    /// term, which the benchmark asserts instead of only reporting.
    pub fn exact_keyword_search(self) -> bool {
        self == EngineKind::Rhizodb
    }

    /// Whether the engine's vector search is exact, so its recall against brute force must be 1.
    pub fn exact_vector_search(self) -> bool {
        matches!(self, EngineKind::Rhizodb | EngineKind::Duckdb)
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
/// unsupported for that workload, which is a result, not a failure. `overrides` replace the
/// text for one engine where its syntax differs from the rest of its dialect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Texts {
    pub gql: Option<&'static str>,
    pub cypher: Option<&'static str>,
    pub sql: Option<&'static str>,
    pub datalog: Option<&'static str>,
    pub overrides: &'static [(EngineKind, &'static str)],
}

impl Texts {
    /// A query that GQL and Cypher both read the same way.
    pub const fn graph(text: &'static str) -> Texts {
        Texts {
            gql: Some(text),
            cypher: Some(text),
            sql: None,
            datalog: None,
            overrides: &[],
        }
    }

    /// Adds the SQL form.
    pub const fn sql(mut self, text: &'static str) -> Texts {
        self.sql = Some(text);
        self
    }

    /// Adds the Datalog form.
    pub const fn datalog(mut self, text: &'static str) -> Texts {
        self.datalog = Some(text);
        self
    }

    /// Replaces the text for the engines listed.
    pub const fn overriding(mut self, overrides: &'static [(EngineKind, &'static str)]) -> Texts {
        self.overrides = overrides;
        self
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

    /// The text `kind` runs: its own override, else the text of its `dialect`.
    pub fn for_engine(
        &self,
        kind: EngineKind,
        dialect: Dialect,
    ) -> Result<&'static str, EngineError> {
        match self.overrides.iter().find(|(engine, _)| *engine == kind) {
            Some((_, text)) => Ok(text),
            None => self.for_dialect(dialect),
        }
    }
}

/// A query parameter in the types both engines take.
#[derive(Clone, Debug, PartialEq)]
pub enum Param {
    Int(i64),
    Text(String),
    Floats(Vec<f64>),
    Ints(Vec<i64>),
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

    /// Adds a list of integers parameter.
    pub fn ints(mut self, name: &'static str, value: &[i64]) -> Self {
        self.params.push((name, Param::Ints(value.to_vec())));
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
    /// The engine cannot do the operation, which is not a failure of the measurement.
    Unsupported(String),
    /// The engine could have done the operation but something it needs is missing here, such as
    /// an extension that was not built or a feature that was not enabled. Reports print it
    /// loudly and tests do not accept it.
    Unavailable(String),
}

impl fmt::Display for EngineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EngineError::Failed(message) => write!(formatter, "{message}"),
            EngineError::Unsupported(message) => write!(formatter, "unsupported: {message}"),
            EngineError::Unavailable(message) => write!(formatter, "unavailable: {message}"),
        }
    }
}

impl std::error::Error for EngineError {}

/// What building an index did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Built {
    /// The engine built a separate index structure, which the build time covers.
    Index,
    /// The engine has no index to build and its search scans the data.
    Scan,
}

/// One connection to an open database. A session runs one query at a time.
pub trait Session {
    /// Parses and plans `text` ahead of time where the engine can, so a later run skips that.
    fn prepare(&mut self, text: &str) -> Result<(), EngineError>;

    /// Runs one query, walks every row it returns, and fingerprints them.
    fn run(&mut self, query: &Query<'_>) -> Result<Fingerprint, EngineError>;

    /// Runs one query and returns the integer in the first column of every row, in the order
    /// the rows came back.
    fn ids(&mut self, query: &Query<'_>) -> Result<Vec<i64>, EngineError>;

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

    /// Builds the engine's own input for [`Instance::bulk_load`] from the dataset, outside any
    /// timing, so the timed load only hands that input to the engine.
    fn prepare_bulk_load(&self, data: &Dataset, scratch: &Path) -> Result<(), EngineError>;

    /// Closes the database, which flushes whatever the engine defers until then.
    fn close(self: Box<Self>) -> Result<(), EngineError> {
        drop(self);
        Ok(())
    }

    /// Loads the whole dataset the way the engine loads fastest, from what
    /// [`Instance::prepare_bulk_load`] built.
    fn bulk_load(&self, data: &Dataset, scratch: &Path) -> Result<(), EngineError>;

    /// Does what search needs that is not index building, such as loading an extension, outside
    /// any timing. Fails loudly when the engine needs something that is missing.
    fn prepare_search(&self) -> Result<(), EngineError> {
        Ok(())
    }

    /// Creates the keyword index over the person `bio`.
    fn create_keyword_index(&self) -> Result<Built, EngineError>;

    /// Creates the vector index over the person `embedding`.
    fn create_vector_index(&self) -> Result<Built, EngineError>;

    /// The query that searches for `$term` and returns person ids, ranked best first.
    fn keyword_search(&self) -> Result<&'static str, EngineError>;

    /// The query that searches for `$vector` by cosine distance and returns person ids, nearest
    /// first.
    fn vector_search(&self) -> Result<&'static str, EngineError>;

    /// Removes everything an engine whose state lives outside `dir` holds, indexes included.
    /// File-backed engines start from a directory and do nothing.
    fn reset(&self) -> Result<(), EngineError> {
        Ok(())
    }
}

/// A database engine under test.
pub trait Engine: Sync {
    /// Which engine this is.
    fn kind(&self) -> EngineKind;

    /// The language the engine's queries are written in.
    fn dialect(&self) -> Dialect;

    /// Opens the database in `dir`, creating it when `dir` holds none. An engine whose state
    /// lives in a server connects to it and ignores `dir`.
    fn open(&self, dir: &Path) -> Result<Box<dyn Instance>, EngineError>;

    /// How the engine is configured and which version ran, for the JSON header. Runs once,
    /// after the benchmark.
    fn config(&self) -> serde_json::Value;

    /// Whether every instance sees one shared state outside its directory, so a workload that
    /// changes it needs the state rebuilt.
    fn shared_state(&self) -> bool {
        self.kind().category() == Category::Server
    }
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
