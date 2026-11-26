use std::sync::MutexGuard;
use std::sync::PoisonError;

use crate::database::Database;
use crate::engine::Epoch;
use crate::engine::changes;
use crate::engine::changes::Decoded;
use crate::engine::changes::EpochId;
use crate::engine::layout::TableId;
use crate::error::Error;
use crate::error::ErrorKind;
use crate::storage::StorageError;
use crate::storage::Table;

const POSITION_FORMAT: u8 = 1;

/// The longest writer id a promotion accepts.
pub(crate) const WRITER_ID_LIMIT: usize = 64;

/// A place in a database's history of changes: the log sequence number of the last change it
/// holds, and the term and writer id of the writer that made that change.
///
/// A database that never held a change, or whose changes were all made before any promotion,
/// is at term 0 with an empty writer id. Two databases at equal positions hold the same graph.
///
/// A host sends a position to another node as the bytes of [`Position::to_bytes`] and reads it
/// back with [`Position::from_bytes`].
///
/// ```
/// use rhizodb::Position;
///
/// let start = Position::start();
/// assert_eq!((start.term(), start.writer_id(), start.lsn()), (0, &[][..], 0));
/// assert_eq!(Position::from_bytes(&start.to_bytes())?, start);
/// # Ok::<(), rhizodb::Error>(())
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Position {
    term: u64,
    writer_id: Vec<u8>,
    lsn: u64,
}

impl Position {
    /// The position of a database that holds no change.
    pub fn start() -> Self {
        Self::default()
    }

    pub(crate) fn new(term: u64, writer_id: &[u8], lsn: u64) -> Result<Self, Error> {
        check_epoch(term, writer_id)?;
        Ok(Self {
            term,
            writer_id: writer_id.to_vec(),
            lsn,
        })
    }

    /// The term of the writer that made the last change, or 0 before any promotion.
    pub fn term(&self) -> u64 {
        self.term
    }

    /// The id of the writer that made the last change, empty before any promotion.
    pub fn writer_id(&self) -> &[u8] {
        &self.writer_id
    }

    /// The log sequence number of the last change, or 0 when there is none.
    pub fn lsn(&self) -> u64 {
        self.lsn
    }

    /// The position as bytes that [`Position::from_bytes`] reads back, in a format that later
    /// releases keep reading.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(18 + self.writer_id.len());
        out.push(POSITION_FORMAT);
        out.extend_from_slice(&self.term.to_le_bytes());
        out.extend_from_slice(&self.lsn.to_le_bytes());
        out.push(self.writer_id.len() as u8);
        out.extend_from_slice(&self.writer_id);
        out
    }

    /// Reads a position from the bytes of [`Position::to_bytes`]. Bytes of another shape fail
    /// with [`ErrorKind::CorruptData`].
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        let malformed = || Error::new(ErrorKind::CorruptData, "the bytes are not a position");
        let (&format, rest) = bytes.split_first().ok_or_else(malformed)?;
        if format != POSITION_FORMAT || rest.len() < 17 {
            return Err(malformed());
        }
        let (term, rest) = rest.split_at(8);
        let (lsn, rest) = rest.split_at(8);
        let (&length, writer_id) = rest.split_first().ok_or_else(malformed)?;
        if writer_id.len() != usize::from(length) {
            return Err(malformed());
        }
        Self::new(le_u64(term), writer_id, le_u64(lsn)).map_err(|_| malformed())
    }
}

impl Position {
    pub(crate) fn of(epoch: EpochId, lsn: u64) -> Self {
        Self {
            term: epoch.term,
            writer_id: epoch.writer_id,
            lsn,
        }
    }

    fn epoch(&self) -> EpochId {
        EpochId {
            term: self.term,
            writer_id: self.writer_id.clone(),
        }
    }
}

/// Whole transactions of one database, in log order, that another database applies with
/// [`Database::apply`]. They come from [`Database::changes_since`] and travel between nodes as
/// the bytes of [`Changes::as_bytes`].
///
/// ```
/// use rhizodb::Changes;
/// use rhizodb::Database;
/// use rhizodb::OpenOptions;
/// use rhizodb::Parameters;
/// use rhizodb::Position;
///
/// let writer = Database::in_memory()?;
/// writer.execute("INSERT (:City {name: 'Oslo'})", &Parameters::new())?;
/// let replica = OpenOptions::new().replica(true).in_memory()?;
/// let changes = writer.changes_since(&replica.position(), 1 << 20)?;
/// let received = Changes::from_bytes(changes.as_bytes())?;
/// assert_eq!(replica.apply(&received)?, writer.position());
/// let rows = replica.execute("MATCH (c:City) RETURN c.name", &Parameters::new())?;
/// assert_eq!(rows.rows().len(), 1);
/// # Ok::<(), rhizodb::Error>(())
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Changes {
    bytes: Vec<u8>,
    decoded: Decoded,
    after: Position,
    last: Position,
}

impl Changes {
    fn decode(bytes: Vec<u8>) -> Result<Self, Error> {
        let decoded = changes::decode(&bytes).map_err(StorageError::from)?;
        let after = Position::of(decoded.after.clone(), decoded.after_lsn);
        let last = Position::of(decoded.last.clone(), decoded.last_lsn());
        Ok(Self {
            bytes,
            decoded,
            after,
            last,
        })
    }

    /// Reads changes from the bytes of [`Changes::as_bytes`], which may come from an untrusted
    /// peer. Bytes that fail a checksum or do not decode fail with [`ErrorKind::CorruptData`].
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        Self::decode(bytes.to_vec())
    }

    /// The changes as bytes that [`Changes::from_bytes`] reads back. Every record in them and
    /// the whole carry checksums.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The position the changes follow, which a database must be at, or past in the same
    /// history, to apply them.
    pub fn after(&self) -> &Position {
        &self.after
    }

    /// The position a database reaches by applying the changes.
    pub fn last(&self) -> &Position {
        &self.last
    }

    /// Whether the changes hold no record, as when the database asked has nothing newer.
    pub fn is_empty(&self) -> bool {
        self.decoded.records.is_empty()
    }

    /// The records the changes hold: one per committed transaction, and one where a writer
    /// began a term.
    pub fn len(&self) -> usize {
        self.decoded.records.len()
    }
}

fn top(epochs: &[Epoch]) -> EpochId {
    epochs
        .last()
        .map(|epoch| EpochId {
            term: epoch.term,
            writer_id: epoch.writer_id.clone(),
        })
        .unwrap_or_default()
}

fn diverged(position: &Position) -> Error {
    Error::new(
        ErrorKind::Diverged,
        format!(
            "LSN {} of term {} is not part of this database's history",
            position.lsn, position.term
        ),
    )
}

impl Database {
    fn roles(&self) -> MutexGuard<'_, ()> {
        self.roles.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The position of the last change this database holds, which every read that begins now
    /// sees.
    pub fn position(&self) -> Position {
        let (epoch, lsn) = self.graph.storage().position();
        Position::of(epoch, lsn)
    }

    /// The whole transactions after `position`, which another database, writer or replica,
    /// reported as its own, up to about `max_bytes` and at least one when there is any. A
    /// replica serves changes too, so they can be relayed from one replica to the next.
    ///
    /// When `position` is this database's own or lies past it in the same term, the changes
    /// are empty. A `position` that is not part of this database's history fails with
    /// [`ErrorKind::Diverged`], and one whose following changes the log no longer keeps fails
    /// with [`ErrorKind::TooFarBehind`], after which the database behind it installs a
    /// snapshot.
    pub fn changes_since(&self, position: &Position, max_bytes: usize) -> Result<Changes, Error> {
        let engine = self.graph.storage();
        let epochs = engine.epochs();
        let (_, held) = engine.position();
        let asked = position.epoch();
        let records = if position.lsn <= held {
            if position.lsn > 0 && crate::engine::epoch_at(&epochs, position.lsn) != asked {
                return Err(diverged(position));
            }
            engine
                .read_changes(position.lsn, max_bytes)
                .map_err(StorageError::from)?
        } else {
            let newest = top(&epochs);
            if asked != newest && asked.term <= newest.term {
                return Err(diverged(position));
            }
            Vec::new()
        };
        let bytes = changes::encode(&asked, position.lsn, &records).map_err(StorageError::from)?;
        Changes::decode(bytes)
    }

    /// Applies `changes` of another database to this replica and returns the position it
    /// reached. Each change is logged and made durable before readers see it, as a commit is.
    /// Changes this replica already holds in the same history are skipped, so applying the same
    /// changes twice is safe.
    ///
    /// It fails with [`ErrorKind::NotReplica`] on a writer, with [`ErrorKind::Gap`] when the
    /// changes start after the last change held, with [`ErrorKind::Fenced`] when they come from
    /// a term older than the newest this database has seen, and with [`ErrorKind::Diverged`]
    /// when this database holds other changes in their place. A failure leaves every change
    /// before it applied.
    pub fn apply(&self, changes: &Changes) -> Result<Position, Error> {
        let _roles = self.roles();
        if !self.graph.is_replica() {
            return Err(Error::new(
                ErrorKind::NotReplica,
                "a writer takes no changes of another writer until it is demoted",
            ));
        }
        let indexes = changes.decoded.touches(TableId::of(Table::Indexes));
        self.graph
            .apply_outside(indexes, |engine| engine.apply_changes(&changes.decoded))?
            .map_err(StorageError::from)?;
        Ok(self.position())
    }

    /// Makes this database the writer of `term` under `writer_id` and returns its position. The
    /// host gives each promotion a term higher than every term the database has seen and a
    /// writer id of 1 to 64 bytes that names the node. A promotion records the new term as a
    /// change of its own, which replicas apply as they apply commits, so every database that
    /// follows it refuses changes of older terms with [`ErrorKind::Fenced`].
    ///
    /// Promoting again with the newest term and writer id records nothing and makes the
    /// database a writer again, as a writer that restarted needs. A replica that never applied a
    /// change also records the graph's format, as opening a new database as a writer does. A lower term fails with
    /// `Fenced`, and the newest term under another writer id fails with
    /// [`ErrorKind::Diverged`].
    pub fn promote(&self, term: u64, writer_id: &[u8]) -> Result<Position, Error> {
        check_epoch(term, writer_id)?;
        if term == 0 {
            return Err(Error::new(
                ErrorKind::InvalidArgumentValue,
                "a promotion needs a term of 1 or more",
            ));
        }
        let _roles = self.roles();
        self.graph
            .maintain(|engine| engine.promote(term, writer_id))?
            .map_err(StorageError::from)?;
        self.graph.set_replica(false);
        self.graph.record_format_if_missing()?;
        Ok(self.position())
    }

    /// Makes this writer a replica: new writes fail with [`ErrorKind::ReplicaReadOnly`], a
    /// write transaction already open may still commit, and once none is open this returns the
    /// final position. The next writer applies the changes up to that position and promotes
    /// with a higher term, so a planned handover loses no write.
    pub fn demote(&self) -> Result<Position, Error> {
        let _roles = self.roles();
        self.graph.maintain(|engine| {
            self.graph.set_replica(true);
            engine.quiesce();
        })?;
        Ok(self.position())
    }
}

/// Damages a batch of valid changes by `bytes`, reseals its checksums, decodes it, applies it to
/// a replica, and reads the replica, for the fuzzer. Fails when decoded changes encode to other
/// bytes. A panic is the failure the fuzzer looks for.
#[cfg(any(test, feature = "fuzzing"))]
pub(crate) fn apply_changes_for_fuzzing(bytes: &[u8]) -> Result<(), String> {
    use std::sync::OnceLock;

    use crate::database::OpenOptions;
    use crate::graph::IndexTarget;
    use crate::result::Parameters;

    static BASE: OnceLock<Result<Vec<u8>, String>> = OnceLock::new();
    let base = BASE.get_or_init(|| {
        let writer = Database::in_memory().map_err(|error| error.to_string())?;
        let run = |query: &str| {
            writer
                .execute(query, &Parameters::new())
                .map(drop)
                .map_err(|error| error.to_string())
        };
        writer
            .promote(1, b"fuzz")
            .map_err(|error| error.to_string())?;
        writer
            .create_keyword_index("text", IndexTarget::Label("A"), &["text"])
            .map_err(|error| error.to_string())?;
        run("UNWIND range(1, 5) AS i INSERT (:A {i: i, text: 'a b'})-[:R {w: i}]->(:B)")?;
        run("MATCH (a:A) WHERE a.i % 2 = 0 SET a.text = 'c', a:C")?;
        run("MATCH (a:A {i: 3}) DETACH DELETE a")?;
        writer
            .changes_since(&Position::start(), 1 << 20)
            .map(|changes| changes.as_bytes().to_vec())
            .map_err(|error| error.to_string())
    });
    let mut damaged = base.clone()?;
    let (whole, edits) = bytes.split_first().unwrap_or((&0, &[]));
    if whole % 8 == 7 {
        damaged = edits.to_vec();
    } else {
        for edit in edits.chunks(3) {
            if let [high, low, value] = edit {
                let at = usize::from(u16::from_be_bytes([*high, *low])) % damaged.len().max(1);
                if let Some(byte) = damaged.get_mut(at) {
                    *byte ^= value;
                }
            }
        }
    }
    let damaged = changes::reseal_for_fuzzing(damaged);
    let Ok(decoded) = Changes::from_bytes(&damaged) else {
        return Ok(());
    };
    if decoded.as_bytes() != damaged.as_slice() {
        return Err("decoded changes keep other bytes".to_owned());
    }
    let replica = OpenOptions::new()
        .replica(true)
        .in_memory()
        .map_err(|error| error.to_string())?;
    let _ = replica.apply(&decoded);
    for probe in [
        "MATCH (n) RETURN n, labels(n) ORDER BY id(n)",
        "MATCH (a)-[r]->(b) RETURN id(a), r, id(b) ORDER BY id(r)",
        "MATCH (n:A) RETURN count(*), sum(n.i)",
        "CALL db.index.keyword.queryNodes('text', 'a c', 10) YIELD node, score RETURN node, score",
    ] {
        let _ = replica.execute(probe, &Parameters::new());
    }
    let _ = replica.check();
    let _ = replica.changes_since(&Position::start(), 1 << 20);
    Ok(())
}

fn le_u64(bytes: &[u8]) -> u64 {
    let mut array = [0; 8];
    array.copy_from_slice(bytes);
    u64::from_le_bytes(array)
}

/// Fails unless `term` and `writer_id` name an epoch: term 0 with no writer id, or a later term
/// with a writer id of 1 to 64 bytes.
pub(crate) fn check_epoch(term: u64, writer_id: &[u8]) -> Result<(), Error> {
    let valid = match term {
        0 => writer_id.is_empty(),
        _ => (1..=WRITER_ID_LIMIT).contains(&writer_id.len()),
    };
    if valid {
        Ok(())
    } else {
        Err(Error::new(
            ErrorKind::InvalidArgumentValue,
            format!(
                "term {term} with a writer id of {} bytes names no epoch: term 0 has no writer id \
                 and every later term has one of 1 to {WRITER_ID_LIMIT} bytes",
                writer_id.len()
            ),
        ))
    }
}

#[cfg(test)]
#[path = "replication.test.rs"]
mod tests;

#[cfg(test)]
#[path = "replication_sim.test.rs"]
mod sim_tests;
