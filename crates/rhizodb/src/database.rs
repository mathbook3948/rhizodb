use std::path::Path;
use std::sync::Arc;

use crate::error::Error;
use crate::graph::Graph;
use crate::procedure::Procedure;
use crate::procedure::Procedures;
use crate::query;
use crate::result::Parameters;
use crate::result::QueryResult;
use crate::storage::redb_backend::RedbStorage;
use crate::transaction::ReadTransaction;
use crate::transaction::WriteTransaction;

/// An embedded graph database. Share one `Database` across threads: reads run concurrently, and
/// writing queries run one at a time.
#[derive(Debug)]
pub struct Database {
    graph: Graph<RedbStorage>,
    procedures: Procedures,
}

impl Database {
    /// Opens the database file at `path`, creating it when it does not exist.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        Ok(Self {
            graph: Graph::open(RedbStorage::open(path)?)?,
            procedures: Procedures::new(),
        })
    }

    /// Creates a database that lives in memory and is gone when dropped.
    pub fn in_memory() -> Result<Self, Error> {
        Ok(Self {
            graph: Graph::open(RedbStorage::in_memory()?)?,
            procedures: Procedures::new(),
        })
    }

    /// Runs one GQL query in its own transaction and returns its rows. To run several queries in
    /// one transaction, use [`Database::begin_read`] or [`Database::begin_write`].
    ///
    /// A query without updating clauses reads a snapshot. A query with updating clauses commits
    /// when it succeeds and leaves no trace when it fails. It waits for an open write
    /// transaction on another thread, and fails with
    /// [`ErrorKind::WriteTransactionOnThisThread`](crate::ErrorKind::WriteTransactionOnThisThread)
    /// when the calling thread holds one.
    pub fn execute(&self, query: &str, parameters: &Parameters) -> Result<QueryResult, Error> {
        query::execute(&self.graph, &self.procedures, query, parameters)
    }

    /// Begins a read transaction, which sees the graph as it was committed at this moment for as
    /// long as it lives. Read transactions run alongside each other and alongside a writer.
    pub fn begin_read(&self) -> Result<ReadTransaction<'_>, Error> {
        Ok(ReadTransaction::new(self.graph.read()?, &self.procedures))
    }

    /// Begins a write transaction. Only one is open at a time, so this waits until any open write
    /// transaction ends, including the one a writing [`Database::execute`] call opens. On the
    /// thread that already holds the open write transaction it fails with
    /// [`ErrorKind::WriteTransactionOnThisThread`](crate::ErrorKind::WriteTransactionOnThisThread)
    /// instead of waiting for itself.
    pub fn begin_write(&self) -> Result<WriteTransaction<'_>, Error> {
        Ok(WriteTransaction::new(self.graph.write()?, &self.procedures))
    }

    /// Makes `procedure` callable with `CALL` by its name, replacing any procedure registered
    /// under the same name. Registration lasts as long as this `Database` value.
    pub fn register_procedure(&mut self, procedure: Procedure) {
        self.procedures
            .insert(procedure.name.clone(), Arc::new(procedure));
    }
}
