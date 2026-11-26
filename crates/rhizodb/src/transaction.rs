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
/// its view. Dropping it ends it. Threads can share it and run queries on it at once.
///
/// ```
/// use rhizodb::Database;
/// use rhizodb::ErrorKind;
/// use rhizodb::Parameters;
///
/// let db = Database::in_memory()?;
/// let snapshot = db.begin_read()?;
/// let error = snapshot.execute("INSERT ()", &Parameters::new()).unwrap_err();
/// assert_eq!(error.kind(), ErrorKind::ReadOnlyTransaction);
/// assert!(snapshot.execute("MATCH (n) RETURN n", &Parameters::new())?.rows().is_empty());
/// # Ok::<(), rhizodb::Error>(())
/// ```
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

    /// Runs one GQL query on the snapshot and returns its rows. A failed query leaves the
    /// transaction usable.
    ///
    /// Fails with the kind that names what is wrong with the query, such as a syntax, binding,
    /// type, or value error, and with
    /// [`ErrorKind::ReadOnlyTransaction`](crate::ErrorKind::ReadOnlyTransaction) for a query that
    /// updates the graph, before it reads data.
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
/// ```
/// use rhizodb::Database;
/// use rhizodb::ErrorKind;
/// use rhizodb::Parameters;
///
/// let db = Database::in_memory()?;
/// let mut tx = db.begin_write()?;
/// tx.execute("INSERT (:Kept)", &Parameters::new())?;
/// assert!(tx.execute("RETURN 1 / 0", &Parameters::new()).is_err());
/// let error = tx.execute("INSERT (:Lost)", &Parameters::new()).unwrap_err();
/// assert_eq!(error.kind(), ErrorKind::TransactionAborted);
/// tx.rollback()?;
/// assert!(db.execute("MATCH (n) RETURN n", &Parameters::new())?.rows().is_empty());
/// # Ok::<(), rhizodb::Error>(())
/// ```
///
/// ```compile_fail
/// fn send<T: Send>(_: T) {}
/// let db = rhizodb::Database::in_memory()?;
/// send(db.begin_write()?);
/// # Ok::<(), rhizodb::Error>(())
/// ```
///
/// ```compile_fail
/// fn share<T: Sync>(_: &T) {}
/// let db = rhizodb::Database::in_memory()?;
/// share(&db.begin_write()?);
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
    /// the whole transaction.
    ///
    /// Fails with the kind that names what is wrong with the query, such as a syntax, binding,
    /// type, or value error, and with
    /// [`ErrorKind::TransactionAborted`](crate::ErrorKind::TransactionAborted) on every call
    /// after a query in the transaction has failed.
    pub fn execute(&mut self, query: &str, parameters: &Parameters) -> Result<QueryResult, Error> {
        let tx = self.tx.as_mut().ok_or_else(aborted)?;
        let result = query::execute_write(tx, &self.began, self.statements, query, parameters);
        if result.is_err() {
            self.tx = None;
        }
        result
    }

    /// Makes the transaction's writes durable and visible to transactions that begin after it.
    ///
    /// Fails with [`ErrorKind::TransactionAborted`](crate::ErrorKind::TransactionAborted) when a
    /// query in the transaction failed, which rolled it back. A commit that fails with
    /// [`ErrorKind::Storage`](crate::ErrorKind::Storage) may or may not have taken effect, as
    /// the crate guide describes.
    pub fn commit(self) -> Result<(), Error> {
        Ok(self.tx.ok_or_else(aborted)?.commit()?)
    }

    /// Discards the transaction's writes. Rolling back a transaction that a failed query already
    /// rolled back succeeds. It adds no [`ErrorKind`]s of its own to the ones every call can fail
    /// with.
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
