use std::path::Path;
use std::sync::Arc;

use crate::error::Error;
use crate::error::ErrorKind;
use crate::graph::Graph;
use crate::graph::IndexTarget;
use crate::graph::VectorMetric;
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
        self.procedures.insert(name.clone(), Arc::new(procedure));
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
    /// Both score every vector in the index.
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

    /// Drops the index `name` and all its entries in one write transaction. Returns whether an
    /// index of that name existed.
    pub fn drop_index(&self, name: &str) -> Result<bool, Error> {
        let mut tx = self.graph.write()?;
        let dropped = tx.drop_index(name)?;
        tx.commit()?;
        Ok(dropped)
    }
}
