use std::path::Path;

use crate::error::Error;
use crate::error::ErrorKind;
use crate::graph::Graph;
use crate::graph::IndexTarget;
use crate::graph::VectorMetric;
use crate::procedure::Procedure;
use crate::query;
use crate::query::Statements;
use crate::result::Parameters;
use crate::result::QueryResult;
#[cfg(not(rhizodb_engine))]
use crate::storage::redb_backend::RedbStorage;
use crate::transaction::ReadTransaction;
use crate::transaction::WriteTransaction;

#[cfg(not(rhizodb_engine))]
pub(crate) type Backend = RedbStorage;

#[cfg(not(rhizodb_engine))]
fn open_backend(path: &Path) -> Result<Backend, Error> {
    Ok(RedbStorage::open(path)?)
}

#[cfg(not(rhizodb_engine))]
fn in_memory_backend() -> Result<Backend, Error> {
    Ok(RedbStorage::in_memory()?)
}

// ast-grep-ignore
// Building with `--cfg rhizodb_engine` runs every test on RhizoDB's own engine instead of redb.
#[cfg(rhizodb_engine)]
pub(crate) type Backend = crate::engine::store::Engine<crate::engine::any_fs::AnyFs>;

#[cfg(rhizodb_engine)]
fn open_backend(path: &Path) -> Result<Backend, Error> {
    let options = crate::engine::store::EngineOptions::default();
    Ok(crate::engine::any_fs::open_path(path, options)
        .map_err(crate::storage::StorageError::from)?)
}

#[cfg(rhizodb_engine)]
fn in_memory_backend() -> Result<Backend, Error> {
    let options = crate::engine::store::EngineOptions::default();
    Ok(crate::engine::any_fs::in_memory(options).map_err(crate::storage::StorageError::from)?)
}

/// An embedded graph database. Share one `Database` across threads: reads run concurrently, and
/// writing queries run one at a time.
#[derive(Debug)]
pub struct Database {
    graph: Graph<Backend>,
    statements: Statements,
}

impl Database {
    /// Opens the database file at `path`, creating it when it does not exist.
    ///
    /// A damaged file fails with [`ErrorKind::CorruptData`](crate::ErrorKind::CorruptData), on
    /// open or on the operation that reads the damage. The storage engine can panic on some
    /// damage. RhizoDB catches that panic, and from then on every operation on this `Database`
    /// fails with `CorruptData` until the file is opened again. Catching needs the host built
    /// with `panic = "unwind"`, the default, and with `panic = "abort"` the process still ends.
    /// RhizoDB installs no panic hook, so the host's hook still reports the caught panic, by
    /// default on standard error.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        Ok(Self {
            graph: Graph::open(open_backend(path.as_ref())?)?,
            statements: Statements::default(),
        })
    }

    /// Creates a database that lives in memory and is gone when dropped.
    pub fn in_memory() -> Result<Self, Error> {
        Ok(Self {
            graph: Graph::open(in_memory_backend()?)?,
            statements: Statements::default(),
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
        query::execute(&self.graph, &self.statements, query, parameters)
    }

    /// Begins a read transaction, which sees the graph as it was committed at this moment for as
    /// long as it lives. Read transactions run alongside each other and alongside a writer.
    pub fn begin_read(&self) -> Result<ReadTransaction<'_>, Error> {
        Ok(ReadTransaction::new(self.graph.read()?, &self.statements))
    }

    /// Begins a write transaction. Only one is open at a time, so this waits until any open write
    /// transaction ends, including the one a writing [`Database::execute`] call opens. On the
    /// thread that already holds the open write transaction it fails with
    /// [`ErrorKind::WriteTransactionOnThisThread`](crate::ErrorKind::WriteTransactionOnThisThread)
    /// instead of waiting for itself.
    pub fn begin_write(&self) -> Result<WriteTransaction<'_>, Error> {
        Ok(WriteTransaction::new(self.graph.write()?, &self.statements))
    }

    /// Makes `procedure` callable with `CALL` by its name, replacing any procedure registered
    /// under the same name. Registration lasts as long as this `Database` value.
    ///
    /// Fails with [`ErrorKind::ReservedProcedureName`](crate::ErrorKind::ReservedProcedureName)
    /// for a name in the `db` namespace, which the built-in procedures reserve.
    pub fn register_procedure(&mut self, procedure: Procedure) -> Result<(), Error> {
        let name = &procedure.signature.name;
        if query::is_reserved(name) {
            return Err(Error::new(
                ErrorKind::ReservedProcedureName,
                format!("procedure `{name}` is in the reserved `db` namespace"),
            ));
        }
        self.statements.register(procedure);
        Ok(())
    }

    /// Creates the keyword index `name` over the string properties `properties` of every node
    /// or relationship that `target` covers, and fills it from the ones already there, in one
    /// write transaction. Text is put in Unicode normalization form NFKC, split into words at
    /// Unicode word boundaries, and lowercased, so a composed accent matches its decomposed
    /// form, and a ligature or a full-width letter matches its plain form. Hangul, Han, kana,
    /// and scripts written without spaces between words, such as Thai, are indexed as
    /// overlapping pairs of letters, so a query finds a word inside a longer run. A letter here
    /// is a grapheme cluster, or in Hangul a syllable or a loose jamo. A property that holds a
    /// list of strings is indexed string by string.
    ///
    /// Search an index over a label with
    /// `CALL db.index.keyword.queryNodes(name, text, limit) YIELD node, score`, and one over a
    /// relationship type with
    /// `CALL db.index.keyword.queryRelationships(name, text, limit) YIELD relationship, score`.
    /// Both match an element that holds any term of `text`, ranked by BM25. Text between two
    /// `"`, as in `"new york" pizza`, is a phrase, which matches only its terms in order and
    /// next to each other inside one string. A word directly followed by `*` outside quotes, as
    /// in `graph*`, matches every term that starts with it. There is no fuzzy matching.
    ///
    /// ```
    /// use rhizodb::Database;
    /// use rhizodb::IndexTarget;
    /// use rhizodb::Parameters;
    ///
    /// let db = Database::in_memory()?;
    /// db.execute(
    ///     "INSERT (:Person)-[:WROTE {note: 'a graph store'}]->(:Paper)",
    ///     &Parameters::new(),
    /// )?;
    /// db.create_keyword_index("notes", IndexTarget::RelationshipType("WROTE"), &["note"])?;
    /// let result = db.execute(
    ///     "CALL db.index.keyword.queryRelationships('notes', 'graph', 10) YIELD relationship \
    ///      RETURN relationship.note",
    ///     &Parameters::new(),
    /// )?;
    /// assert_eq!(result.rows().len(), 1);
    /// # Ok::<(), rhizodb::Error>(())
    /// ```
    ///
    /// Creating an index that exists with the same definition changes nothing. It fails with
    /// [`ErrorKind::IndexAlreadyExists`](crate::ErrorKind::IndexAlreadyExists) when an index of
    /// another definition holds the name, and with
    /// [`ErrorKind::InvalidIndexDefinition`](crate::ErrorKind::InvalidIndexDefinition) for an
    /// empty name or no properties.
    pub fn create_keyword_index(
        &self,
        name: &str,
        target: IndexTarget<'_>,
        properties: &[&str],
    ) -> Result<(), Error> {
        let mut tx = self.graph.write()?;
        tx.create_keyword_index(name, target, properties)?;
        Ok(tx.commit()?)
    }

    /// Creates the vector index `name` over the property `property` of every node or
    /// relationship that `target` covers, and fills it from the ones already there, in one
    /// write transaction. An element is indexed when the property is a list of `dimension`
    /// finite numbers whose length leaves every score finite, and under
    /// [`VectorMetric::Cosine`] not zero.
    ///
    /// Search an index over a label with
    /// `CALL db.index.vector.queryNodes(name, vector, limit) YIELD node, score`, and one over a
    /// relationship type with
    /// `CALL db.index.vector.queryRelationships(name, vector, limit) YIELD relationship, score`.
    /// Search is exact: each query reads and scores every vector in the index, so its cost
    /// grows with the number of indexed vectors times their dimension. There is no approximate
    /// index. A host with more vectors than one query can scan keeps an approximate index of
    /// its own outside the database and reads the elements it returns by ID, for example with
    /// `MATCH (n) WHERE id(n) IN $ids` or `MATCH ()-[r]->() WHERE id(r) IN $ids`.
    ///
    /// Creating an index that exists with the same definition changes nothing. It fails with
    /// [`ErrorKind::IndexAlreadyExists`](crate::ErrorKind::IndexAlreadyExists) when an index of
    /// another definition holds the name, and with
    /// [`ErrorKind::InvalidIndexDefinition`](crate::ErrorKind::InvalidIndexDefinition) for an
    /// empty name or a dimension of zero.
    pub fn create_vector_index(
        &self,
        name: &str,
        target: IndexTarget<'_>,
        property: &str,
        dimension: usize,
        metric: VectorMetric,
    ) -> Result<(), Error> {
        let mut tx = self.graph.write()?;
        tx.create_vector_index(name, target, property, dimension, metric)?;
        Ok(tx.commit()?)
    }

    /// Creates the property index `name` over the property `property` of every node or
    /// relationship that `target` covers, and fills it from the ones already there, in one
    /// write transaction. A query that looks a property up by value reads the elements the
    /// index holds for that value instead of every element of the label or type, when it names
    /// the property in the pattern, as in `MATCH (p:Person {id: $id})`, or compares it in the
    /// `WHERE` of the `MATCH` with `=`, `IN`, `<`, `<=`, `>`, or `>=`, as in
    /// `MATCH (p:Person) WHERE p.age >= 18 AND p.age < 30`, against a value that the pattern
    /// does not bind. A pattern that starts at a node of the label, or at a relationship of the
    /// type, uses it.
    ///
    /// Values are found under the same rules as `=` and `<`: an integer finds the float of the
    /// same number, a string sorts by its bytes, and a time or datetime compares by its instant.
    /// NaN, which equals nothing, is not indexed.
    ///
    /// ```
    /// use rhizodb::Database;
    /// use rhizodb::IndexTarget;
    /// use rhizodb::Parameters;
    /// use rhizodb::Value;
    ///
    /// let db = Database::in_memory()?;
    /// db.create_property_index("person_id", IndexTarget::Label("Person"), "id")?;
    /// db.execute("INSERT (:Person {id: 1, name: 'Ada'}), (:Person {id: 2.0, name: 'Grace'})", &Parameters::new())?;
    /// let parameters = Parameters::from([("id".to_owned(), Value::Integer(2))]);
    /// let result = db.execute("MATCH (p:Person {id: $id}) RETURN p.name", &parameters)?;
    /// assert_eq!(result.rows(), [vec![Value::String("Grace".to_owned())]]);
    /// # Ok::<(), rhizodb::Error>(())
    /// ```
    ///
    /// Creating an index that exists with the same definition changes nothing. It fails with
    /// [`ErrorKind::IndexAlreadyExists`](crate::ErrorKind::IndexAlreadyExists) when an index of
    /// another definition holds the name, and with
    /// [`ErrorKind::InvalidIndexDefinition`](crate::ErrorKind::InvalidIndexDefinition) for an
    /// empty name.
    pub fn create_property_index(
        &self,
        name: &str,
        target: IndexTarget<'_>,
        property: &str,
    ) -> Result<(), Error> {
        let mut tx = self.graph.write()?;
        tx.create_property_index(name, target, property)?;
        Ok(tx.commit()?)
    }

    /// Drops the index `name` and all its entries in one write transaction. Returns whether an
    /// index of that name existed.
    pub fn drop_index(&self, name: &str) -> Result<bool, Error> {
        let mut tx = self.graph.write()?;
        let dropped = tx.drop_index(name)?;
        tx.commit()?;
        Ok(dropped)
    }
}

#[cfg(test)]
#[path = "database_crash.test.rs"]
mod crash_tests;
