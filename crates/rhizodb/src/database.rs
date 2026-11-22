use std::path::Path;

use crate::error::Error;
use crate::graph::Graph;
use crate::query;
use crate::result::Parameters;
use crate::result::QueryResult;
use crate::storage::redb_backend::RedbStorage;

/// An embedded graph database. Share one `Database` across threads: reads run concurrently, and
/// writing queries run one at a time.
#[derive(Debug)]
pub struct Database {
    graph: Graph<RedbStorage>,
}

impl Database {
    /// Opens the database file at `path`, creating it when it does not exist.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        Ok(Self {
            graph: Graph::open(RedbStorage::open(path)?)?,
        })
    }

    /// Creates a database that lives in memory and is gone when dropped.
    pub fn in_memory() -> Result<Self, Error> {
        Ok(Self {
            graph: Graph::open(RedbStorage::in_memory()?)?,
        })
    }

    /// Runs one GQL query in its own transaction and returns its rows.
    ///
    /// A query without updating clauses reads a snapshot. A query with updating clauses commits
    /// when it succeeds and leaves no trace when it fails.
    pub fn execute(&self, query: &str, parameters: &Parameters) -> Result<QueryResult, Error> {
        query::execute(&self.graph, query, parameters)
    }
}
