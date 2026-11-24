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
use crate::storage::Maintenance;
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

/// How to open a database file, for settings beyond what [`Database::open`] chooses. Settings
/// start at what [`Database::open`] uses, and later releases may add more.
///
/// ```
/// use rhizodb::ErrorKind;
/// use rhizodb::OpenOptions;
///
/// let dir = tempfile::tempdir()?;
/// let path = dir.path().join("graph.redb");
/// let missing = OpenOptions::new().create(false).open(&path).unwrap_err();
/// assert_eq!(missing.kind(), ErrorKind::DatabaseNotFound);
/// let created = OpenOptions::new().open(&path)?;
/// drop(created);
/// OpenOptions::new().create(false).open(&path)?;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Clone, Debug)]
#[must_use]
pub struct OpenOptions {
    create: bool,
}

impl Default for OpenOptions {
    fn default() -> Self {
        Self { create: true }
    }
}

impl OpenOptions {
    /// The settings [`Database::open`] uses.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether [`OpenOptions::open`] creates the file when there is none, which it does unless
    /// this is set to `false`. Without it, a path with no file fails with
    /// [`ErrorKind::DatabaseNotFound`](crate::ErrorKind::DatabaseNotFound).
    pub fn create(mut self, create: bool) -> Self {
        self.create = create;
        self
    }

    /// Opens the database file at `path` with these settings, as [`Database::open`] describes.
    pub fn open(&self, path: impl AsRef<Path>) -> Result<Database, Error> {
        let path = path.as_ref();
        if !self.create && !exists(path)? {
            return Err(Error::new(
                ErrorKind::DatabaseNotFound,
                format!("there is no database file at {}", path.display()),
            ));
        }
        Ok(Database {
            graph: Graph::open(open_backend(path)?)?,
            statements: Statements::default(),
        })
    }
}

fn exists(path: &Path) -> Result<bool, Error> {
    path.try_exists().map_err(|error| {
        Error::caused_by(
            ErrorKind::Storage,
            format!("cannot read whether {} exists: {error}", path.display()),
            Box::new(error),
        )
    })
}

/// The size of a database on disk and how well its cache serves reads, from
/// [`Database::stats`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct DatabaseStats {
    /// The bytes of the database file.
    pub file_bytes: u64,
    /// The bytes of the pages and values that the file's last checkpoint reaches. The rest of
    /// the file is free space that later checkpoints fill and [`Database::compact`] gives back.
    pub live_bytes: u64,
    /// The extents of the file that hold data.
    pub live_extents: u64,
    /// The extents inside the file that hold nothing, which later writes reuse and
    /// [`Database::compact`] cuts off when they end the file.
    pub free_extents: u64,
    /// The bytes of the write-ahead log beside the file, which a checkpoint empties.
    pub wal_bytes: u64,
    /// The page reads the cache served since the database was opened.
    pub cache_hits: u64,
    /// The page reads the cache could not serve, which read the file, since the database was
    /// opened.
    pub cache_misses: u64,
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
    ///
    /// It is the same as [`OpenOptions::new`] followed by [`OpenOptions::open`].
    ///
    /// ```
    /// use rhizodb::Database;
    /// use rhizodb::ErrorKind;
    /// use rhizodb::Parameters;
    ///
    /// let directory = tempfile::tempdir()?;
    /// let path = directory.path().join("graph.rhizodb");
    /// let db = Database::open(&path)?;
    /// db.execute("INSERT (:Person {name: 'Ada'})", &Parameters::new())?;
    /// assert_eq!(Database::open(&path).unwrap_err().kind(), ErrorKind::Storage);
    /// drop(db);
    /// let reopened = Database::open(&path)?;
    /// let result = reopened.execute("MATCH (p:Person) RETURN count(p)", &Parameters::new())?;
    /// assert_eq!(result.rows(), [vec![rhizodb::Value::Integer(1)]]);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        OpenOptions::new().open(path)
    }

    #[cfg(test)]
    pub(crate) fn graph(&self) -> &Graph<Backend> {
        &self.graph
    }

    /// Creates a database that lives in memory and is gone when dropped.
    ///
    /// ```
    /// let db = rhizodb::Database::in_memory()?;
    /// db.execute("INSERT ()", &rhizodb::Parameters::new())?;
    /// # Ok::<(), rhizodb::Error>(())
    /// ```
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
    ///
    /// ```
    /// use rhizodb::Database;
    /// use rhizodb::ErrorKind;
    /// use rhizodb::Parameters;
    /// use rhizodb::Value;
    ///
    /// let db = Database::in_memory()?;
    /// let created = db.execute("INSERT (n:Task {done: false}) RETURN id(n)", &Parameters::new())?;
    /// assert_eq!(created.rows(), [vec![Value::Integer(0)]]);
    /// let error = db.execute("INSERT (:Task {done: 1 / 0})", &Parameters::new()).unwrap_err();
    /// assert_eq!(error.kind(), ErrorKind::DivisionByZero);
    /// let count = db.execute("MATCH (t:Task) RETURN count(t)", &Parameters::new())?;
    /// assert_eq!(count.rows(), [vec![Value::Integer(1)]]);
    /// # Ok::<(), rhizodb::Error>(())
    /// ```
    pub fn execute(&self, query: &str, parameters: &Parameters) -> Result<QueryResult, Error> {
        query::execute(&self.graph, &self.statements, query, parameters)
    }

    /// Begins a read transaction, which sees the graph as it was committed at this moment for as
    /// long as it lives. Read transactions run alongside each other and alongside a writer.
    ///
    /// ```
    /// use rhizodb::Database;
    /// use rhizodb::Parameters;
    /// use rhizodb::Value;
    ///
    /// let db = Database::in_memory()?;
    /// let snapshot = db.begin_read()?;
    /// db.execute("INSERT (:Late)", &Parameters::new())?;
    /// let seen = snapshot.execute("MATCH (n:Late) RETURN count(n)", &Parameters::new())?;
    /// assert_eq!(seen.rows(), [vec![Value::Integer(0)]]);
    /// # Ok::<(), rhizodb::Error>(())
    /// ```
    pub fn begin_read(&self) -> Result<ReadTransaction<'_>, Error> {
        Ok(ReadTransaction::new(self.graph.read()?, &self.statements))
    }

    /// Begins a write transaction. Only one is open at a time, so this waits until any open write
    /// transaction ends, including the one a writing [`Database::execute`] call opens. On the
    /// thread that already holds the open write transaction it fails with
    /// [`ErrorKind::WriteTransactionOnThisThread`](crate::ErrorKind::WriteTransactionOnThisThread)
    /// instead of waiting for itself.
    ///
    /// ```
    /// use rhizodb::Database;
    /// use rhizodb::ErrorKind;
    /// use rhizodb::Parameters;
    ///
    /// let db = Database::in_memory()?;
    /// let mut tx = db.begin_write()?;
    /// tx.execute("INSERT (:Draft)", &Parameters::new())?;
    /// assert_eq!(db.begin_write().unwrap_err().kind(), ErrorKind::WriteTransactionOnThisThread);
    /// tx.commit()?;
    /// db.begin_write()?.rollback()?;
    /// # Ok::<(), rhizodb::Error>(())
    /// ```
    pub fn begin_write(&self) -> Result<WriteTransaction<'_>, Error> {
        Ok(WriteTransaction::new(self.graph.write()?, &self.statements))
    }

    /// Writes a copy of the committed database to a new file at `path`, which
    /// [`Database::open`] then opens as this database, so restoring a backup is opening it.
    ///
    /// The copy holds exactly the commits that finished before the backup began, read from one
    /// snapshot as a [`ReadTransaction`] reads it. Readers and writers carry on while it runs,
    /// and commits made meanwhile stay out of the copy. The copy is written under a staging name
    /// that starts with `.` and the file name of `path`, in the same directory, synced, and then
    /// linked to `path`, so after a crash or a failure `path` holds either nothing or the whole
    /// copy. A crash can leave the staging file behind, and removing it is safe. An in-memory
    /// database backs up to a file the same way.
    ///
    /// It fails with [`ErrorKind::Storage`](crate::ErrorKind::Storage) when a file or directory
    /// already exists at `path`, which is never replaced, or when writing the copy fails.
    ///
    /// ```
    /// use rhizodb::Database;
    /// use rhizodb::Parameters;
    /// use rhizodb::Value;
    ///
    /// let directory = tempfile::tempdir()?;
    /// let db = Database::in_memory()?;
    /// db.execute("INSERT (:Person {name: 'Ada'})", &Parameters::new())?;
    ///
    /// let backup = directory.path().join("backup.rhizodb");
    /// db.backup_to(&backup)?;
    ///
    /// let restored = Database::open(&backup)?;
    /// let result = restored.execute("MATCH (p:Person) RETURN p.name", &Parameters::new())?;
    /// assert_eq!(result.rows(), [vec![Value::String("Ada".to_owned())]]);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn backup_to(&self, path: impl AsRef<Path>) -> Result<(), Error> {
        Ok(self.graph.backup_to(path.as_ref())?)
    }

    /// Writes every index definition, node, and relationship of the committed database to `out`
    /// as a dump: UTF-8 text with one JSON object per line, read back by [`Database::import`].
    ///
    /// The dump holds exactly the commits that finished before the export began, read from one
    /// snapshot as a [`ReadTransaction`] reads it, while readers and writers carry on. It keeps
    /// node and relationship IDs, and the next ID of each, so references a host keeps by ID
    /// stay valid after an import. It keeps every value's exact type: an integer apart from a
    /// float of the same number, `-0.0` apart from `0.0`, NaN and the infinities by their bits,
    /// and each temporal value with its offset or zone. The crate's guide describes the format
    /// and how its versions stay compatible.
    ///
    /// It fails with [`ErrorKind::Io`](crate::ErrorKind::Io) when writing to `out` fails, and
    /// `out` may then hold part of a dump, which [`Database::import`] refuses because its end
    /// line is missing.
    ///
    /// ```
    /// use rhizodb::Database;
    /// use rhizodb::Parameters;
    /// use rhizodb::Value;
    ///
    /// let db = Database::in_memory()?;
    /// db.execute("INSERT (:Person {name: 'Ada', born: 1815})", &Parameters::new())?;
    /// let mut dump = Vec::new();
    /// db.export(&mut dump)?;
    ///
    /// let copy = Database::in_memory()?;
    /// copy.import(dump.as_slice())?;
    /// let result = copy.execute("MATCH (p:Person) RETURN p.name, p.born", &Parameters::new())?;
    /// assert_eq!(
    ///     result.rows(),
    ///     [vec![Value::String("Ada".to_owned()), Value::Integer(1815)]]
    /// );
    /// # Ok::<(), rhizodb::Error>(())
    /// ```
    pub fn export(&self, out: impl std::io::Write) -> Result<(), Error> {
        crate::dump::export(&self.graph.read()?, out)
    }

    /// Reads a dump that [`Database::export`] wrote into this database, which must be new: it
    /// must hold no index and never have held a node or a relationship, or the import fails with
    /// [`ErrorKind::DatabaseNotEmpty`](crate::ErrorKind::DatabaseNotEmpty).
    ///
    /// Nodes and relationships keep their IDs, and the next IDs are those of the exported
    /// database, so an ID deleted before the export is not given again. Indexes are created
    /// first and filled as the elements arrive.
    ///
    /// The import commits a batch of lines at a time, so its memory stays bounded by the batch
    /// and the longest line, however large the dump. A failed import therefore leaves the
    /// batches before the failure committed. Import into a new file and delete it when the
    /// import fails, rather than into a database other code uses.
    ///
    /// It fails with [`ErrorKind::InvalidDump`](crate::ErrorKind::InvalidDump) for a malformed
    /// or truncated dump, with
    /// [`ErrorKind::UnsupportedDumpVersion`](crate::ErrorKind::UnsupportedDumpVersion) for one
    /// written in a newer format version, and with [`ErrorKind::Io`](crate::ErrorKind::Io) when
    /// reading `input` fails.
    ///
    /// ```
    /// use std::io::BufReader;
    ///
    /// use rhizodb::Database;
    /// use rhizodb::ErrorKind;
    /// use rhizodb::Parameters;
    /// use rhizodb::Value;
    ///
    /// let directory = tempfile::tempdir()?;
    /// let path = directory.path().join("graph.jsonl");
    /// let db = Database::in_memory()?;
    /// db.execute("INSERT (:A)-[:R]->(:B)", &Parameters::new())?;
    /// db.export(std::fs::File::create(&path)?)?;
    ///
    /// let restored = Database::in_memory()?;
    /// restored.import(BufReader::new(std::fs::File::open(&path)?))?;
    /// let result = restored.execute("MATCH (a:A)-[r:R]->(b:B) RETURN id(a), id(r), id(b)", &Parameters::new())?;
    /// assert_eq!(result.rows(), [vec![Value::Integer(0), Value::Integer(0), Value::Integer(1)]]);
    /// let again = restored.import(BufReader::new(std::fs::File::open(&path)?)).unwrap_err();
    /// assert_eq!(again.kind(), ErrorKind::DatabaseNotEmpty);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn import(&self, input: impl std::io::BufRead) -> Result<(), Error> {
        crate::dump::import(&self.graph, input)
    }

    /// Makes `procedure` callable with `CALL` by its name, replacing any procedure registered
    /// under the same name. Registration lasts as long as this `Database` value. Queries that
    /// already started keep the procedures they began with.
    ///
    /// Fails with [`ErrorKind::ReservedProcedureName`](crate::ErrorKind::ReservedProcedureName)
    /// for a name in the `db` namespace, which the built-in procedures reserve.
    pub fn register_procedure(&self, procedure: Procedure) -> Result<(), Error> {
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
    ///
    /// ```
    /// use rhizodb::Database;
    /// use rhizodb::IndexTarget;
    /// use rhizodb::Parameters;
    /// use rhizodb::Value;
    /// use rhizodb::VectorMetric;
    ///
    /// let db = Database::in_memory()?;
    /// db.create_vector_index("embedding", IndexTarget::Label("Doc"), "vec", 3, VectorMetric::Euclidean)?;
    /// db.execute(
    ///     "INSERT (:Doc {title: 'near', vec: [1.0, 1.0, 1.0]}), (:Doc {title: 'far', vec: [9, 9, 9]})",
    ///     &Parameters::new(),
    /// )?;
    /// let result = db.execute(
    ///     "CALL db.index.vector.queryNodes('embedding', [1.0, 1.0, 0.0], 1) YIELD node, score \
    ///      RETURN node.title, score",
    ///     &Parameters::new(),
    /// )?;
    /// assert_eq!(result.rows(), [vec![Value::String("near".to_owned()), Value::Float(-1.0)]]);
    /// # Ok::<(), rhizodb::Error>(())
    /// ```
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

    /// Writes every committed transaction into the database file, moves the live data into as
    /// little of the file as it needs, and shortens the file to fit. Reads go on while it runs,
    /// and writing transactions wait for it.
    ///
    /// Fails with [`ErrorKind::WriteTransactionOnThisThread`] when the calling thread holds a
    /// write transaction, and with [`ErrorKind::Unsupported`] on a database stored by redb.
    pub fn compact(&self) -> Result<(), Error> {
        Ok(self.graph.maintain(Maintenance::compact)??)
    }

    /// The size of the database on disk and how well its cache serves reads.
    ///
    /// Fails with [`ErrorKind::Unsupported`] on a database stored by redb.
    pub fn stats(&self) -> Result<DatabaseStats, Error> {
        let stats = self.graph.maintain(Maintenance::stats)??;
        Ok(DatabaseStats {
            file_bytes: stats.file_bytes,
            live_bytes: stats.live_bytes,
            live_extents: stats.live_extents,
            free_extents: stats.free_extents,
            wal_bytes: stats.wal_bytes,
            cache_hits: stats.cache_hits,
            cache_misses: stats.cache_misses,
        })
    }

    /// Reads every page that the database file's last checkpoint reaches and checks its
    /// checksum, its key order, and every count the file records, and fails with
    /// [`ErrorKind::CorruptData`] on the first that does not hold. Commits since that checkpoint
    /// are checked as they are read back from the log.
    ///
    /// Fails with [`ErrorKind::WriteTransactionOnThisThread`] when the calling thread holds a
    /// write transaction, and with [`ErrorKind::Unsupported`] on a database stored by redb.
    pub fn check(&self) -> Result<(), Error> {
        Ok(self.graph.maintain(Maintenance::check)??)
    }

    /// Drops the index `name` and all its entries in one write transaction. Returns whether an
    /// index of that name existed.
    ///
    /// ```
    /// use rhizodb::Database;
    /// use rhizodb::IndexTarget;
    ///
    /// let db = Database::in_memory()?;
    /// db.create_property_index("by_id", IndexTarget::Label("Item"), "id")?;
    /// assert!(db.drop_index("by_id")?);
    /// assert!(!db.drop_index("by_id")?);
    /// # Ok::<(), rhizodb::Error>(())
    /// ```
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

#[cfg(test)]
#[path = "database_backup.test.rs"]
mod backup_tests;
