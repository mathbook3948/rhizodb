use std::io::Read;
use std::path::Path;
use std::sync::Mutex;

use crate::engine::Role;
use crate::engine::any_fs;
use crate::engine::any_fs::AnyFs;
use crate::engine::store::Engine;
use crate::engine::store::EngineOptions;
use crate::error::Error;
use crate::error::ErrorKind;
use crate::graph::Access;
use crate::graph::Graph;
use crate::graph::IndexTarget;
use crate::graph::VectorMetric;
use crate::procedure::Procedure;
use crate::query;
use crate::query::Limits;
use crate::query::Parallelism;
use crate::query::Statements;
use crate::replication::Position;
use crate::result::Parameters;
use crate::result::QueryResult;
use crate::snapshot::Snapshot;
use crate::storage::Maintenance;
use crate::storage::StorageError;
use crate::transaction::ReadTransaction;
use crate::transaction::WriteTransaction;

pub(crate) type Backend = Engine<AnyFs>;

fn open_backend(path: &Path, options: EngineOptions) -> Result<Backend, Error> {
    any_fs::open_path(path, options).map_err(|error| {
        StorageError::from(error)
            .in_context(|| format!("cannot open the database at {}", path.display()))
            .into()
    })
}

fn open_backend_shared(path: &Path, options: EngineOptions) -> Result<Backend, Error> {
    any_fs::open_path_shared(path, options).map_err(|error| {
        StorageError::from(error)
            .in_context(|| format!("cannot open the database at {}", path.display()))
            .into()
    })
}

fn open_backend_read_only(path: &Path, options: EngineOptions) -> Result<Backend, Error> {
    Ok(any_fs::open_path_read_only(path, options).map_err(StorageError::from)?)
}

fn in_memory_backend(options: EngineOptions) -> Result<Backend, Error> {
    Ok(any_fs::in_memory(options).map_err(StorageError::from)?)
}

fn peer_id(peer: &[u8]) -> Result<&[u8], Error> {
    if (1..=PEER_ID_LIMIT).contains(&peer.len()) {
        return Ok(peer);
    }
    Err(Error::new(
        ErrorKind::InvalidPeerId,
        format!(
            "a peer id holds 1 to {PEER_ID_LIMIT} bytes, not {}",
            peer.len()
        ),
    ))
}

const PEER_ID_LIMIT: usize = 64;

/// How to open a database file, for settings beyond what [`Database::open`] chooses. Settings
/// start at what [`Database::open`] uses, and later releases may add more.
///
/// ```
/// use rhizodb::ErrorKind;
/// use rhizodb::OpenOptions;
///
/// let dir = tempfile::tempdir()?;
/// let path = dir.path().join("graph.rhizodb");
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
    read_only: bool,
    multi_process: bool,
    replica: bool,
    parallelism: Parallelism,
    engine: EngineOptions,
    force_install: bool,
    limits: Limits,
    max_dump_line: usize,
}

impl Default for OpenOptions {
    fn default() -> Self {
        Self {
            create: true,
            read_only: false,
            multi_process: false,
            replica: false,
            parallelism: Parallelism::default(),
            engine: EngineOptions::default(),
            force_install: false,
            limits: Limits::default(),
            max_dump_line: usize::MAX,
        }
    }
}

impl OpenOptions {
    /// The settings [`Database::open`] uses.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether [`OpenOptions::open`] creates the file when there is none, which it does unless
    /// this is set to `false`. Without it, a path with no file or an empty file fails with
    /// [`ErrorKind::DatabaseNotFound`](crate::ErrorKind::DatabaseNotFound).
    pub fn create(mut self, create: bool) -> Self {
        self.create = create;
        self
    }

    /// Whether the database opens read-only, which it does not unless this is set to `true`. A
    /// read-only open takes a shared lock on the file, so any number of processes read the file
    /// at once, while an open that writes fails with
    /// [`ErrorKind::DatabaseAlreadyOpen`](crate::ErrorKind::DatabaseAlreadyOpen) until every one
    /// of them is dropped, and a read-only open fails the same way while an open that writes
    /// holds the file. A process holds one open of a file at a time, so a second read-only open
    /// in the same process fails with `DatabaseAlreadyOpen` too; share the one `Database`
    /// across threads instead.
    ///
    /// It never creates, writes, renames, or removes a file but one, whatever
    /// [`OpenOptions::create`] says: a path with no file or an empty file fails with
    /// [`ErrorKind::DatabaseNotFound`](crate::ErrorKind::DatabaseNotFound). After an unclean
    /// shutdown it replays the write-ahead log into memory and leaves the log as it is, so reads
    /// see every commit the log holds, once it has synced the log so that no power loss takes
    /// back a commit a read saw. On Windows, where only a handle with write access can sync a
    /// file, a read-only open of a file that was not closed cleanly fails with `ReadOnly`
    /// instead, and opening it once for writing recovers it. Reads, [`Database::export`],
    /// [`Database::snapshot`], [`Database::backup_to`], [`Database::stats`], and
    /// [`Database::check`] work, and every
    /// call that would change the file fails with
    /// [`ErrorKind::ReadOnly`](crate::ErrorKind::ReadOnly): queries that update the graph, write
    /// transactions, index changes, [`Database::import`], [`Database::compact`], and the calls
    /// that write for replication. A file written in an older format, or whose keyword indexes
    /// another build's tokenizer made, fails to open with `ReadOnly`, since only an open that
    /// writes upgrades it. With [`OpenOptions::replica`], a writer's file still fails with
    /// [`ErrorKind::NotReplica`](crate::ErrorKind::NotReplica), and a file that holds no change
    /// fails with `ReadOnly`, since marking it a replica is a write. [`Database::role`] reports
    /// the role the file records, such as `Writer` for a writer's file, though every write fails.
    /// [`Database::snapshot`] fails with `ReadOnly` after a crash cut a checkpoint between its
    /// stages, until an open that writes finishes it.
    ///
    /// The one file it creates and writes is the side file named after the database file with
    /// `-shm`, which every process that has the file open read-only maps to share the state of
    /// their session, created with the database file's permissions. The last of them to close
    /// removes it, and it is never needed to read the database. Where the directory refuses to
    /// create it, as on read-only media, or on a file system that is not local, the open reads
    /// the file alone instead, leaves no side file, and still shares the file with any number
    /// of such opens. A process of another build fails to
    /// join a running session with `DatabaseAlreadyOpen`, and an open that finds every slot of
    /// the session taken fails with
    /// [`ErrorKind::TooManyProcesses`](crate::ErrorKind::TooManyProcesses). A read-only open of
    /// an earlier release refuses a file while a side file lies beside it.
    ///
    /// ```
    /// use rhizodb::Database;
    /// use rhizodb::ErrorKind;
    /// use rhizodb::OpenOptions;
    /// use rhizodb::Parameters;
    /// use rhizodb::Value;
    ///
    /// let directory = tempfile::tempdir()?;
    /// let path = directory.path().join("graph.rhizodb");
    /// Database::open(&path)?.execute("INSERT (:City {name: 'Oslo'})", &Parameters::new())?;
    /// let db = OpenOptions::new().read_only(true).open(&path)?;
    /// let result = db.execute("MATCH (c:City) RETURN c.name", &Parameters::new())?;
    /// assert_eq!(result.rows(), [vec![Value::String("Oslo".to_owned())]]);
    /// let error = db.execute("INSERT (:City)", &Parameters::new()).unwrap_err();
    /// assert_eq!(error.kind(), ErrorKind::ReadOnly);
    /// assert_eq!(Database::open(&path).unwrap_err().kind(), ErrorKind::DatabaseAlreadyOpen);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }

    /// Whether a database opened for writing lets processes that open the file
    /// [read-only](OpenOptions::read_only) read it while this one writes, which it does not
    /// unless this is set to `true`.
    ///
    /// Such an open takes part in the file's multi-process session, as every read-only open
    /// does, through the side file named after the database file with `-shm`. One process of
    /// the session writes at a time: a second open for writing in the session fails with
    /// [`ErrorKind::DatabaseAlreadyOpen`](crate::ErrorKind::DatabaseAlreadyOpen), as does an
    /// open for writing without this setting while any process of the session has the file
    /// open. A read-only process sees the commits made before it opened the file, and its
    /// checkpoints never reuse space that a read-only process still reads.
    ///
    /// The file must lie on a local file system of this machine that takes byte-range locks
    /// and shared mappings, and the directory must take the side file, or the open fails with
    /// [`ErrorKind::Storage`](crate::ErrorKind::Storage). A writer that closes while read-only
    /// processes still have the file open leaves its log for the next open that writes, which
    /// replays it, and writes nothing else. It has no effect on a database in memory or one
    /// opened read-only.
    ///
    /// ```
    /// use rhizodb::Database;
    /// use rhizodb::ErrorKind;
    /// use rhizodb::OpenOptions;
    /// use rhizodb::Parameters;
    ///
    /// let directory = tempfile::tempdir()?;
    /// let path = directory.path().join("graph.rhizodb");
    /// let writer = OpenOptions::new().multi_process(true).open(&path)?;
    /// writer.execute("INSERT (:City {name: 'Oslo'})", &Parameters::new())?;
    /// assert_eq!(Database::open(&path).unwrap_err().kind(), ErrorKind::DatabaseAlreadyOpen);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn multi_process(mut self, multi_process: bool) -> Self {
        self.multi_process = multi_process;
        self
    }

    /// Whether the database opens as a replica, which it does not unless this is set to
    /// `true`. A replica serves reads as any database does, and refuses every local write for
    /// the whole file with [`ErrorKind::NotWriter`](crate::ErrorKind::NotWriter): queries that
    /// update the graph, write transactions, index changes, and imports.
    ///
    /// A file remembers its [role](crate::Role), so this setting matters only for a file that
    /// holds no change yet, which becomes a replica and stays one, without writing the graph's
    /// format version, until it applies its writer's first change. A file that already holds a
    /// writer's changes fails to open with [`ErrorKind::NotReplica`](crate::ErrorKind::NotReplica)
    /// unless that writer was demoted, and a file written in an older format opens as a replica
    /// that waits for its writer's upgrade, refusing graph reads with
    /// [`ErrorKind::FormatUpgradePending`](crate::ErrorKind::FormatUpgradePending) until it
    /// applies that upgrade, since the upgrade is a write. Data that belongs to the node itself
    /// goes in a database file of its own.
    ///
    /// ```
    /// use rhizodb::ErrorKind;
    /// use rhizodb::OpenOptions;
    /// use rhizodb::Parameters;
    /// use rhizodb::Role;
    ///
    /// let replica = OpenOptions::new().replica(true).in_memory()?;
    /// assert_eq!(replica.role(), Role::Replica);
    /// assert!(replica.execute("MATCH (n) RETURN n", &Parameters::new())?.rows().is_empty());
    /// let error = replica.execute("INSERT ()", &Parameters::new()).unwrap_err();
    /// assert_eq!(error.kind(), ErrorKind::NotWriter);
    /// # Ok::<(), rhizodb::Error>(())
    /// ```
    pub fn replica(mut self, replica: bool) -> Self {
        self.replica = replica;
        self
    }

    /// How many threads one read query may run on, which defaults to what
    /// [`std::thread::available_parallelism`] reports. A query that reads enough start nodes
    /// splits them across these threads, the calling thread among them, and returns the same rows
    /// in the same order, and fails the same way, as on one thread. A query that calls a procedure
    /// the host registered, updates the graph, or runs in a write transaction stays on the calling
    /// thread. Zero and one keep every query on the calling thread. The threads are a budget the
    /// database's read queries share: a query splits only across the threads that other read
    /// queries of the database leave idle, so many concurrent queries each stay on their own
    /// thread instead of crowding the cores.
    ///
    /// ```
    /// use rhizodb::OpenOptions;
    /// use rhizodb::Parameters;
    /// use rhizodb::Value;
    ///
    /// let db = OpenOptions::new().threads(4).parallel_threshold(2).in_memory()?;
    /// db.execute("UNWIND range(1, 100) AS i INSERT (:N {i: i})", &Parameters::new())?;
    /// let result = db.execute("MATCH (n:N) RETURN sum(n.i)", &Parameters::new())?;
    /// assert_eq!(result.rows(), [vec![Value::Integer(5050)]]);
    /// # Ok::<(), rhizodb::Error>(())
    /// ```
    pub fn threads(mut self, threads: usize) -> Self {
        self.parallelism.threads = threads;
        self
    }

    /// How many start nodes a match must read before a read query splits them across
    /// [threads](OpenOptions::threads), so that a small query does not pay to start them. It
    /// defaults to 8192. Zero and one act as two, since a match of one start node has nothing to
    /// split.
    pub fn parallel_threshold(mut self, nodes: usize) -> Self {
        self.parallelism.threshold = nodes;
        self
    }

    /// How many bytes of write-ahead log the database keeps beyond what its own recovery needs,
    /// for peers that [acknowledged](Database::acknowledge) a position and for
    /// [snapshots](Database::snapshot) still being read. It defaults to 1 GiB. When the kept
    /// log passes it, the database stops keeping the log for the peers furthest behind, oldest
    /// first, until the rest fits, and those peers then need a new snapshot. Zero keeps no log
    /// for peers at all.
    pub fn max_retained_bytes(mut self, bytes: u64) -> Self {
        self.engine.max_retained_bytes = bytes;
        self
    }

    /// Whether [`OpenOptions::install_snapshot`] replaces a database it would otherwise keep,
    /// which it does not unless this is set to `true`: one that has seen a newer term of its
    /// history than the snapshot holds, refused with
    /// [`ErrorKind::Fenced`](crate::ErrorKind::Fenced), and one that holds the changes of
    /// another database, refused with
    /// [`ErrorKind::DifferentDatabase`](crate::ErrorKind::DifferentDatabase), and one whose
    /// history of terms cannot be read, refused with
    /// [`ErrorKind::CorruptData`](crate::ErrorKind::CorruptData). A snapshot of an older term
    /// comes from a writer that a later one replaced, so a host forces it only to roll a node
    /// back on purpose, forces another database only to reuse the file for it, and forces a
    /// damaged database to resync it from the snapshot.
    pub fn force_install(mut self, force: bool) -> Self {
        self.force_install = force;
        self
    }

    /// How long a [`Snapshot`] may hold the pages of its checkpoint, which defaults to one hour.
    /// While a snapshot lives the file cannot reuse the space of pages that later commits
    /// rewrite, so the file grows by what they rewrite; [`DatabaseStats::held_bytes`] reports
    /// that space. Past the limit the database releases the snapshot, and reading it fails with
    /// [`ErrorKind::SnapshotExpired`](crate::ErrorKind::SnapshotExpired). Set it above the
    /// longest transfer the host expects. A limit past what the clock can reach, such as
    /// [`Duration::MAX`](std::time::Duration::MAX), never releases a snapshot.
    pub fn snapshot_time_limit(mut self, limit: std::time::Duration) -> Self {
        self.engine.pin_limit = limit;
        self
    }

    /// How many list items and map entries one value that a query builds may hold, counted
    /// through every list and map nested in it, which defaults to 10,000,000. It also caps how
    /// many elements `range()` gives. An operator, function, list comprehension, or `collect()`
    /// whose value would pass it fails with
    /// [`ErrorKind::NumberOutOfRange`](crate::ErrorKind::NumberOutOfRange), so one query cannot
    /// exhaust the host's memory by growing a value. Values the host passes as parameters or
    /// stores as properties are not limited by it until a query builds a larger value from them.
    ///
    /// ```
    /// use rhizodb::ErrorKind;
    /// use rhizodb::OpenOptions;
    /// use rhizodb::Parameters;
    ///
    /// let db = OpenOptions::new().max_value_items(1_000).in_memory()?;
    /// let none = Parameters::new();
    /// db.execute("RETURN [x IN range(1, 10) | range(1, 99)]", &none)?;
    /// let error = db
    ///     .execute("RETURN [x IN range(1, 10) | range(1, 100)]", &none)
    ///     .unwrap_err();
    /// assert_eq!(error.kind(), ErrorKind::NumberOutOfRange);
    /// # Ok::<(), rhizodb::Error>(())
    /// ```
    pub fn max_value_items(mut self, items: usize) -> Self {
        self.limits.items = items;
        self
    }

    /// How many bytes of strings and byte strings one value that a query builds may hold,
    /// counted through every list and map nested in it, which defaults to 256 MiB. A value that
    /// would pass it fails as [`OpenOptions::max_value_items`] describes.
    pub fn max_value_bytes(mut self, bytes: usize) -> Self {
        self.limits.bytes = bytes;
        self
    }

    /// How many bytes one line of a dump may hold for [`Database::import`], counting a carriage
    /// return before its line feed but not the line feed, which is not limited by default. A
    /// line that passes it fails with [`ErrorKind::InvalidDump`](crate::ErrorKind::InvalidDump)
    /// once that many bytes are read, before the rest of it is read or parsed. Importing a line
    /// takes up to about 100 times its length in memory at its peak, as for a node with many
    /// short property names or a long list of one-element lists, so a host that imports dumps it
    /// did not write sets this to what it can afford divided by about 100, and one that imports
    /// only its own exports leaves it.
    pub fn max_dump_line_bytes(mut self, bytes: usize) -> Self {
        self.max_dump_line = bytes;
        self
    }

    /// Creates a database with these settings that lives in memory and is gone when dropped, as
    /// [`Database::in_memory`] describes.
    ///
    /// Fails with [`ErrorKind::DatabaseNotFound`](crate::ErrorKind::DatabaseNotFound) when
    /// [`OpenOptions::read_only`] is `true`, since a database in memory starts with nothing to
    /// read.
    pub fn in_memory(&self) -> Result<Database, Error> {
        if self.read_only {
            return Err(Error::new(
                ErrorKind::DatabaseNotFound,
                "a database in memory starts empty, so there is none to open read-only",
            ));
        }
        self.database(graph_on(in_memory_backend(self.engine)?, self.replica)?)
    }

    /// Opens the database file at `path` with these settings, as [`Database::open`] describes.
    ///
    /// Fails with [`ErrorKind::DatabaseNotFound`](crate::ErrorKind::DatabaseNotFound) when
    /// [`OpenOptions::create`] is `false` or [`OpenOptions::read_only`] is `true` and there is
    /// no file at `path` or the file there is empty, and with the kinds [`Database::open`]
    /// lists.
    pub fn open(&self, path: impl AsRef<Path>) -> Result<Database, Error> {
        let path = path.as_ref();
        if (!self.create || self.read_only) && !holds_bytes(path)? {
            return Err(Error::new(
                ErrorKind::DatabaseNotFound,
                format!("there is no database file at {}", path.display()),
            ));
        }
        let engine = EngineOptions {
            build: crate::graph::build_identity(),
            ..self.engine
        };
        let backend = if self.read_only {
            open_backend_read_only(path, engine)?
        } else if self.multi_process {
            open_backend_shared(path, engine)?
        } else {
            open_backend(path, engine)?
        };
        self.database(graph_on(backend, self.replica)?)
    }

    /// Writes the snapshot that `input` streams to the database file at `path` with these
    /// settings and opens it, as [`Database::install_snapshot`] describes.
    ///
    /// Fails with [`ErrorKind::ReadOnly`](crate::ErrorKind::ReadOnly) when
    /// [`OpenOptions::read_only`] is `true`, since an install writes the file.
    pub fn install_snapshot(
        &self,
        path: impl AsRef<Path>,
        input: impl Read,
    ) -> Result<Database, Error> {
        if self.read_only {
            return Err(Error::new(
                ErrorKind::ReadOnly,
                "a snapshot cannot be installed read-only, since the install writes the file",
            ));
        }
        let installed = any_fs::install_path(path.as_ref(), input, self.engine, self.force_install)
            .map_err(StorageError::from)?;
        self.database(graph_on(installed, true)?)
    }

    fn database(&self, graph: Graph<Backend>) -> Result<Database, Error> {
        let mut database =
            Database::from_parts(graph, Statements::new(self.parallelism, self.limits));
        database.max_dump_line = self.max_dump_line;
        Ok(database)
    }
}

/// The graph over `backend`, which becomes a replica first when `replica` asks for one. A file
/// that is not the writer, or that is open read-only, opens without writing.
pub(crate) fn graph_on(backend: Backend, replica: bool) -> Result<Graph<Backend>, Error> {
    if replica {
        backend.declare_replica().map_err(StorageError::from)?;
    }
    let access = if backend.is_read_only() {
        Access::ReadOnly
    } else if backend.role() != Role::Writer {
        Access::Replica
    } else {
        Access::Writer
    };
    Ok(Graph::open_as(backend, access)?)
}

fn holds_bytes(path: &Path) -> Result<bool, Error> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(metadata.len() > 0),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(Error::caused_by(
            ErrorKind::Storage,
            format!("cannot read whether {} exists", path.display()),
            Box::new(error),
        )),
    }
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
    /// The bytes the write-ahead log beside the file takes on disk. A checkpoint removes the
    /// segments it no longer needs, and the segment in use keeps up to 1 MiB allocated ahead.
    pub wal_bytes: u64,
    /// The reads of pages and long values that the cache served since the database was opened.
    pub cache_hits: u64,
    /// The reads of pages and long values that the cache could not serve, which read the file,
    /// since the database was opened.
    pub cache_misses: u64,
    /// The bytes of pages that checkpoints have replaced but the file cannot reuse yet, since a
    /// [`ReadTransaction`] or a [`Snapshot`] still reads them.
    pub held_bytes: u64,
}

/// An embedded graph database. Share one `Database` across threads: reads run concurrently, and
/// writing queries run one at a time.
#[derive(Debug)]
pub struct Database {
    pub(crate) graph: Graph<Backend>,
    statements: Statements,
    pub(crate) roles: Mutex<()>,
    max_dump_line: usize,
}

impl Database {
    pub(crate) fn from_parts(graph: Graph<Backend>, statements: Statements) -> Self {
        Self {
            graph,
            statements,
            roles: Mutex::new(()),
            max_dump_line: usize::MAX,
        }
    }

    /// Opens the database file at `path`, creating it when it does not exist.
    ///
    /// Fails with
    /// [`ErrorKind::DatabaseAlreadyOpen`](crate::ErrorKind::DatabaseAlreadyOpen) when another
    /// `Database`, in this process or another, holds the file on a file system with file locks,
    /// and with
    /// [`ErrorKind::UnsupportedFormatVersion`](crate::ErrorKind::UnsupportedFormatVersion) when
    /// the file records a format version newer than this build reads, or older than the oldest
    /// it upgrades. A damaged file fails with
    /// [`ErrorKind::CorruptData`](crate::ErrorKind::CorruptData), on open or on the operation that
    /// reads the damage, since every page carries a checksum.
    ///
    /// The log segments beside the file are named after it, found through `path` with every
    /// symlink and `..` resolved, so every path to the file finds the same log. On Unix a file
    /// with more than one hard link fails with [`ErrorKind::Storage`](crate::ErrorKind::Storage),
    /// since each of its names would find its own log.
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
    /// assert_eq!(Database::open(&path).unwrap_err().kind(), ErrorKind::DatabaseAlreadyOpen);
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

    /// Creates a database that lives in memory and is gone when dropped. It adds no
    /// [`ErrorKind`]s of its own to the ones every call can fail with.
    ///
    /// ```
    /// let db = rhizodb::Database::in_memory()?;
    /// db.execute("INSERT ()", &rhizodb::Parameters::new())?;
    /// # Ok::<(), rhizodb::Error>(())
    /// ```
    pub fn in_memory() -> Result<Self, Error> {
        OpenOptions::new().in_memory()
    }

    /// Runs one GQL query in its own transaction and returns its rows. To run several queries in
    /// one transaction, use [`Database::begin_read`] or [`Database::begin_write`].
    ///
    /// A query without updating clauses reads a snapshot. A query with updating clauses commits
    /// when it succeeds and leaves no trace when it fails. It waits for an open write
    /// transaction on another thread.
    ///
    /// Fails with the kind that names what is wrong with the query, such as a syntax, binding,
    /// type, or value error, and with
    /// [`ErrorKind::WriteTransactionOnThisThread`](crate::ErrorKind::WriteTransactionOnThisThread)
    /// when a query with updating clauses runs on a thread that holds a write transaction.
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
    /// long as it lives. Read transactions run alongside each other and alongside a writer. It
    /// adds no [`ErrorKind`]s of its own to the ones every call can fail with.
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
    /// transaction ends, including the one a writing [`Database::execute`] call opens.
    ///
    /// Fails with
    /// [`ErrorKind::WriteTransactionOnThisThread`](crate::ErrorKind::WriteTransactionOnThisThread)
    /// on the thread that already holds the open write transaction, instead of waiting for
    /// itself.
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
    /// copy. On a file system without hard links, the staging file is renamed to `path` by a
    /// rename that fails when `path` exists, on Linux, Android, and Apple platforms. A crash can
    /// leave the staging file behind, and removing it is safe. An in-memory database backs up to
    /// a file the same way.
    ///
    /// Fails with [`ErrorKind::BackupTargetExists`](crate::ErrorKind::BackupTargetExists) when a
    /// file or directory already exists at `path`, which is never replaced. A `path` that names
    /// no file, one whose directory cannot be written, or one on a file system with neither hard
    /// links nor such a rename, as on other platforms or many FUSE servers, fails with
    /// [`ErrorKind::Storage`](crate::ErrorKind::Storage), and the I/O error behind it is the
    /// error's [`source`](std::error::Error::source).
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

    /// Pins the database as its last checkpoint left it and returns it as a [`Snapshot`] to
    /// stream to another node, which installs it with [`Database::install_snapshot`] and then
    /// follows the log from the snapshot's position. Commits after that checkpoint stay out of
    /// the snapshot and reach the receiver through the log. Taking a snapshot forces no
    /// checkpoint, and readers and writers carry on while it is read.
    ///
    /// While the snapshot lives, the database keeps every page of its checkpoint and the log
    /// after its position, as [`OpenOptions::max_retained_bytes`] bounds, so the file and the log
    /// grow by what commits rewrite meanwhile. A database that serves as a replica takes
    /// snapshots the same way, so a replica can seed further replicas.
    ///
    /// ```
    /// use rhizodb::Database;
    /// use rhizodb::Parameters;
    /// use rhizodb::Value;
    ///
    /// let directory = tempfile::tempdir()?;
    /// let source = Database::open(directory.path().join("source.rhizodb"))?;
    /// source.execute("INSERT (:Person {name: 'Ada'})", &Parameters::new())?;
    /// source.compact()?;
    ///
    /// let snapshot = source.snapshot()?;
    /// let mut stream = vec![0; snapshot.len() as usize];
    /// let mut offset = 0;
    /// while offset < stream.len() {
    ///     offset += snapshot.read_at(offset as u64, &mut stream[offset..])?;
    /// }
    ///
    /// let replica = Database::install_snapshot(directory.path().join("replica.rhizodb"), stream.as_slice())?;
    /// let result = replica.execute("MATCH (p:Person) RETURN p.name", &Parameters::new())?;
    /// assert_eq!(result.rows(), [vec![Value::String("Ada".to_owned())]]);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn snapshot(&self) -> Result<Snapshot<'_>, Error> {
        let export = self
            .graph
            .storage()
            .export(&[])
            .map_err(StorageError::from)?;
        Ok(Snapshot::new(export))
    }

    /// Writes the snapshot that `input` streams, as [`Snapshot::read_at`] reads it, to the
    /// database file at `path`, and opens that file as a replica with the default settings, as
    /// [`OpenOptions::replica`] describes, since it holds its writer's changes up to the
    /// snapshot's position and follows them from there.
    ///
    /// The snapshot is written under a staging name beside `path`, synced, and checked page by
    /// page before it is renamed over `path`, so after a crash or a failure `path` holds the
    /// database it held before, or none, or the whole snapshot. A database at `path` is replaced
    /// together with its log, and must not be open: the install then fails with
    /// [`ErrorKind::DatabaseAlreadyOpen`](crate::ErrorKind::DatabaseAlreadyOpen) and leaves it as
    /// it was.
    ///
    /// It fails with [`ErrorKind::CorruptData`](crate::ErrorKind::CorruptData) when a chunk of
    /// the stream fails its checksum, comes from another offset or another snapshot, or the
    /// stream ends early, or when the database at `path` or its log is too damaged to tell
    /// which terms it has seen, which [`OpenOptions::force_install`] resyncs, and with [`ErrorKind::Io`](crate::ErrorKind::Io) when reading `input`
    /// fails. A transfer that was cut off can be retried with a reader that resumes from the
    /// offset it reached, since the snapshot gives the same bytes at every offset.
    ///
    /// On Unix the rename is synced into its directory before this returns. On other platforms
    /// the directory cannot be synced, so a crash right after an install may leave the database
    /// that was there before, and on Windows another open may take the file between the rename
    /// and this open, which then fails with
    /// [`ErrorKind::DatabaseAlreadyOpen`](crate::ErrorKind::DatabaseAlreadyOpen).
    ///
    /// It is the same as [`OpenOptions::new`] followed by [`OpenOptions::install_snapshot`].
    pub fn install_snapshot(path: impl AsRef<Path>, input: impl Read) -> Result<Self, Error> {
        OpenOptions::new().install_snapshot(path, input)
    }

    /// Keeps the write-ahead log after `position` for the peer named `peer`, replacing the
    /// position it kept for that peer before, so the peer can read the changes after it with
    /// `changes_since`. Call it with the position a peer reports once it holds the changes up to
    /// there, and with a snapshot's position before that snapshot is dropped.
    /// Peers are named by 1 to 64 bytes the host chooses. The database file records each
    /// peer's position with every checkpoint and when it closes, so the peers and the log kept
    /// for them survive a restart. After a crash the file keeps each peer at the position the
    /// last checkpoint recorded, and a peer first acknowledged after it is forgotten.
    ///
    /// The kept log is bounded by [`OpenOptions::max_retained_bytes`], past which the peers
    /// furthest behind are released and need a new snapshot. Call
    /// [`Database::forget_peer`] for a peer that is gone, so it keeps nothing.
    ///
    /// A position of a newer term than this database has seen tells it that another writer
    /// took over, so a writer becomes fenced, as [`Database::promote`] describes for a newer
    /// term.
    ///
    /// Fails with [`ErrorKind::InvalidPeerId`](crate::ErrorKind::InvalidPeerId) for an empty
    /// peer id or one longer than 64 bytes, and with
    /// [`ErrorKind::TooManyPeers`](crate::ErrorKind::TooManyPeers) for a new peer when 32 are
    /// kept.
    pub fn acknowledge(&self, peer: &[u8], position: &Position) -> Result<(), Error> {
        let peer = peer_id(peer)?;
        let storage = self.graph.storage();
        storage
            .observe_term(position.term())
            .map_err(StorageError::from)?;
        Ok(storage
            .acknowledge(peer, position.lsn())
            .map_err(StorageError::from)?)
    }

    /// Stops keeping the write-ahead log for the peer named `peer`, and returns whether it kept
    /// any.
    ///
    /// Fails with [`ErrorKind::InvalidPeerId`](crate::ErrorKind::InvalidPeerId) for an empty
    /// peer id or one longer than 64 bytes.
    pub fn forget_peer(&self, peer: &[u8]) -> Result<bool, Error> {
        let peer = peer_id(peer)?;
        let storage = self.graph.storage();
        storage
            .writable("forget a peer")
            .map_err(StorageError::from)?;
        Ok(storage.forget(peer))
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
    /// Fails with [`ErrorKind::Io`](crate::ErrorKind::Io) when writing to `out` fails, and `out`
    /// may then hold part of a dump, which [`Database::import`] refuses because its end line is
    /// missing.
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
    /// must hold no index and never have held a node or a relationship.
    ///
    /// Nodes and relationships keep their IDs, and the next IDs are those of the exported
    /// database, so an ID deleted before the export is not given again. Indexes are created
    /// first and filled as the elements arrive.
    ///
    /// The import commits a batch of lines at a time, so its memory stays bounded by the batch
    /// and the longest line, however large the dump. [`OpenOptions::max_dump_line_bytes`]
    /// bounds that line. Write transactions and writing queries on
    /// other threads wait until the import ends, so none lands between two batches, while
    /// reads go on and see the batches committed so far. A failed import therefore leaves the
    /// batches before the failure committed. Import into a new file and delete it when the
    /// import fails, rather than into a database other code uses.
    ///
    /// Fails with [`ErrorKind::DatabaseNotEmpty`](crate::ErrorKind::DatabaseNotEmpty) when the
    /// database is not new, with [`ErrorKind::InvalidDump`](crate::ErrorKind::InvalidDump) for a
    /// malformed or truncated dump, with
    /// [`ErrorKind::UnsupportedDumpVersion`](crate::ErrorKind::UnsupportedDumpVersion) for one
    /// written in a newer format version, with [`ErrorKind::Io`](crate::ErrorKind::Io) when
    /// reading `input` fails, and with
    /// [`ErrorKind::WriteTransactionOnThisThread`](crate::ErrorKind::WriteTransactionOnThisThread)
    /// when the calling thread holds a write transaction.
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
        crate::dump::import(&self.graph, input, self.max_dump_line)
    }

    /// Makes `procedure` callable with `CALL` by its name, replacing any procedure registered
    /// under the same name. Registration lasts as long as this `Database` value. Queries that
    /// already started keep the procedures they began with.
    ///
    /// Fails with [`ErrorKind::ReservedProcedureName`](crate::ErrorKind::ReservedProcedureName)
    /// for a name in the `db` namespace, which the built-in procedures reserve, and with no other
    /// kind, since it touches no data.
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
    /// Creating an index that exists with the same definition changes nothing. Fails with
    /// [`ErrorKind::IndexAlreadyExists`](crate::ErrorKind::IndexAlreadyExists) when an index of
    /// another definition holds the name, with
    /// [`ErrorKind::InvalidIndexDefinition`](crate::ErrorKind::InvalidIndexDefinition) for an
    /// empty name or no properties, and with
    /// [`ErrorKind::WriteTransactionOnThisThread`](crate::ErrorKind::WriteTransactionOnThisThread)
    /// when the calling thread holds a write transaction.
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
    /// Creating an index that exists with the same definition changes nothing. Fails with
    /// [`ErrorKind::IndexAlreadyExists`](crate::ErrorKind::IndexAlreadyExists) when an index of
    /// another definition holds the name, with
    /// [`ErrorKind::InvalidIndexDefinition`](crate::ErrorKind::InvalidIndexDefinition) for an
    /// empty name or a dimension of zero or above `u32::MAX`, and with
    /// [`ErrorKind::WriteTransactionOnThisThread`](crate::ErrorKind::WriteTransactionOnThisThread)
    /// when the calling thread holds a write transaction.
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
    /// Creating an index that exists with the same definition changes nothing. Fails with
    /// [`ErrorKind::IndexAlreadyExists`](crate::ErrorKind::IndexAlreadyExists) when an index of
    /// another definition holds the name, with
    /// [`ErrorKind::InvalidIndexDefinition`](crate::ErrorKind::InvalidIndexDefinition) for an
    /// empty name, and with
    /// [`ErrorKind::WriteTransactionOnThisThread`](crate::ErrorKind::WriteTransactionOnThisThread)
    /// when the calling thread holds a write transaction.
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
    /// Fails with
    /// [`ErrorKind::WriteTransactionOnThisThread`](crate::ErrorKind::WriteTransactionOnThisThread)
    /// when the calling thread holds a write transaction.
    pub fn compact(&self) -> Result<(), Error> {
        Ok(self.graph.maintain(Maintenance::compact)??)
    }

    /// The size of the database on disk and how well its cache serves reads. It waits for no
    /// transaction, so it also answers on a thread that holds a write transaction.
    pub fn stats(&self) -> Result<DatabaseStats, Error> {
        let stats = self.graph.inspect(Maintenance::stats)?;
        Ok(DatabaseStats {
            file_bytes: stats.file_bytes,
            live_bytes: stats.live_bytes,
            live_extents: stats.live_extents,
            free_extents: stats.free_extents,
            wal_bytes: stats.wal_bytes,
            cache_hits: stats.cache_hits,
            cache_misses: stats.cache_misses,
            held_bytes: stats.held_bytes,
        })
    }

    /// Reads every page that the database file's last checkpoint reaches and checks its
    /// checksum, its key order, and every count the file records. Commits since that checkpoint
    /// are checked as they are read back from the log. Then it checks every committed
    /// relationship record against the checksum of the chunk that stores it.
    ///
    /// Fails with [`ErrorKind::CorruptData`](crate::ErrorKind::CorruptData) on the first page,
    /// count, or chunk that does not hold, and with
    /// [`ErrorKind::WriteTransactionOnThisThread`](crate::ErrorKind::WriteTransactionOnThisThread)
    /// when the calling thread holds a write transaction.
    ///
    /// It also checks a database that waits for its writer's upgrade, as
    /// [`ErrorKind::FormatUpgradePending`](crate::ErrorKind::FormatUpgradePending) describes,
    /// whose relationship chunks it skips, since an older format stored them without checksums.
    pub fn check(&self) -> Result<(), Error> {
        self.graph.maintain(Maintenance::check)??;
        Ok(self.graph.check_relationship_chunks()?)
    }

    /// Drops the index `name` and all its entries in one write transaction. Returns whether an
    /// index of that name existed, so a missing index is not an error.
    ///
    /// Fails with
    /// [`ErrorKind::WriteTransactionOnThisThread`](crate::ErrorKind::WriteTransactionOnThisThread)
    /// when the calling thread holds a write transaction.
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

#[cfg(test)]
#[path = "database_format.test.rs"]
mod format_tests;

#[cfg(test)]
#[path = "database_read_only.test.rs"]
mod read_only_tests;
