use std::fmt;
use std::marker::PhantomData;

use crate::database::Backend;
use crate::error::Error;
use crate::error::ErrorKind;
use crate::graph::GraphTx;
use crate::query;
use crate::query::Statements;
use crate::result::Parameters;
use crate::result::QueryResult;
use crate::storage::Storage;
use crate::temporal;
use crate::temporal::DateTime;

/// A snapshot of the graph that several queries read, begun with
/// [`Database::begin_read`](crate::Database::begin_read). Commits made after it began stay out of
/// its view. Dropping it ends it.
#[derive(Debug)]
pub struct ReadTransaction<'db> {
    tx: GraphTx<<Backend as Storage>::Read<'db>>,
    began: DateTime,
    statements: &'db Statements,
}

impl<'db> ReadTransaction<'db> {
    pub(crate) fn new(
        tx: GraphTx<<Backend as Storage>::Read<'db>>,
        statements: &'db Statements,
    ) -> Self {
        Self {
            tx,
            began: temporal::now(),
            statements,
        }
    }

    /// Runs one GQL query on the snapshot and returns its rows. A query that updates the graph
    /// fails with [`ErrorKind::ReadOnlyTransaction`] before it reads data. A failed query leaves
    /// the transaction usable.
    pub fn execute(&self, query: &str, parameters: &Parameters) -> Result<QueryResult, Error> {
        query::execute_read(&self.tx, &self.began, self.statements, query, parameters)
    }
}

/// A transaction whose queries take effect together, begun with
/// [`Database::begin_write`](crate::Database::begin_write). Its queries see its own writes, and
/// other transactions see them only after [`WriteTransaction::commit`]. Dropping it without a
/// commit rolls it back.
///
/// It stays on the thread that began it, so a second write begun on that thread can fail
/// instead of waiting for itself. A host that holds a write across `.await` points runs it on
/// one thread, for example in `spawn_blocking`.
///
/// ```compile_fail
/// fn send<T: Send>(_: T) {}
/// let db = rhizodb::Database::in_memory()?;
/// send(db.begin_write()?);
/// # Ok::<(), rhizodb::Error>(())
/// ```
pub struct WriteTransaction<'db> {
    tx: Option<GraphTx<<Backend as Storage>::Write<'db>>>,
    began: DateTime,
    statements: &'db Statements,
    on_its_thread: PhantomData<*const ()>,
}

impl<'db> WriteTransaction<'db> {
    pub(crate) fn new(
        tx: GraphTx<<Backend as Storage>::Write<'db>>,
        statements: &'db Statements,
    ) -> Self {
        Self {
            tx: Some(tx),
            began: temporal::now(),
            statements,
            on_its_thread: PhantomData,
        }
    }

    /// Runs one GQL query in the transaction and returns its rows. A query that fails rolls back
    /// the whole transaction, so every later call fails with [`ErrorKind::TransactionAborted`].
    pub fn execute(&mut self, query: &str, parameters: &Parameters) -> Result<QueryResult, Error> {
        let tx = self.tx.as_mut().ok_or_else(aborted)?;
        let result = query::execute_write(tx, &self.began, self.statements, query, parameters);
        if result.is_err() {
            self.tx = None;
        }
        result
    }

    /// Makes the transaction's writes durable and visible to transactions that begin after it.
    pub fn commit(self) -> Result<(), Error> {
        Ok(self.tx.ok_or_else(aborted)?.commit()?)
    }

    /// Discards the transaction's writes. Rolling back a transaction that a failed query already
    /// rolled back succeeds.
    pub fn rollback(self) -> Result<(), Error> {
        match self.tx {
            Some(tx) => Ok(tx.rollback()?),
            None => Ok(()),
        }
    }
}

impl fmt::Debug for WriteTransaction<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WriteTransaction")
            .field("aborted", &self.tx.is_none())
            .finish_non_exhaustive()
    }
}

fn aborted() -> Error {
    Error::new(
        ErrorKind::TransactionAborted,
        "the transaction was rolled back after a query in it failed",
    )
}
