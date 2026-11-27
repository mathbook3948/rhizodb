use std::sync::MutexGuard;
use std::sync::PoisonError;
use std::time::Duration;

use crate::database::Database;
use crate::engine::Epoch;
use crate::engine::Role as EngineRole;
use crate::engine::changes;
use crate::engine::changes::Decoded;
use crate::engine::changes::EpochId;
use crate::engine::epoch_at;
use crate::engine::error::EngineError;
use crate::engine::error::Failure;
use crate::engine::layout::TableId;
use crate::engine::wal_format::Body;
use crate::engine::wal_format::Op;
use crate::error::Error;
use crate::error::ErrorKind;
use crate::graph::GraphError;
use crate::graph::newer_format_put;
use crate::graph::older_format_put;
use crate::storage::StorageError;
use crate::storage::Table;

const POSITION_FORMAT: u8 = 3;
const HISTORY_FORMAT: u8 = 2;

/// The longest writer id a promotion accepts.
pub(crate) const WRITER_ID_LIMIT: usize = 64;

/// A place in a database's history of changes: the log sequence number of the last change it
/// holds, the term and writer id of the writer that made that change, a chain hash over every
/// change up to it, and the id of the database the history belongs to.
///
/// A database that never held a change, or whose changes were all made before any promotion,
/// is at term 0 with an empty writer id. Two databases at equal positions hold the same graph,
/// and the chain hash tells apart two histories that reused a term, a writer id, and an LSN.
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
    chain: u64,
    database_id: u128,
}

impl Position {
    /// The position of a database that holds no change.
    pub fn start() -> Self {
        Self::default()
    }

    #[cfg(test)]
    pub(crate) fn new(term: u64, writer_id: &[u8], lsn: u64) -> Result<Self, Error> {
        check_epoch(term, writer_id)?;
        Ok(Self {
            term,
            writer_id: writer_id.to_vec(),
            lsn,
            chain: 0,
            database_id: 0,
        })
    }

    pub(crate) fn of(epoch: EpochId, lsn: u64, chain: u64) -> Self {
        Self {
            term: epoch.term,
            writer_id: epoch.writer_id,
            lsn,
            chain,
            database_id: 0,
        }
    }

    pub(crate) fn in_database(self, database_id: u128) -> Self {
        Self {
            database_id,
            ..self
        }
    }

    fn epoch(&self) -> EpochId {
        EpochId {
            term: self.term,
            writer_id: self.writer_id.clone(),
        }
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

    /// The chain hash of the history up to the last change, or 0 when there is none.
    pub fn chain(&self) -> u64 {
        self.chain
    }

    /// The id of the database whose history this is, the same on a writer and every database
    /// that follows it, or 0 for a database that holds no change.
    pub fn database_id(&self) -> u128 {
        self.database_id
    }

    /// The position as bytes that [`Position::from_bytes`] reads back, in a format that later
    /// releases keep reading.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(42 + self.writer_id.len());
        out.push(POSITION_FORMAT);
        out.extend_from_slice(&self.database_id.to_le_bytes());
        out.extend_from_slice(&self.term.to_le_bytes());
        out.extend_from_slice(&self.lsn.to_le_bytes());
        out.extend_from_slice(&self.chain.to_le_bytes());
        out.push(self.writer_id.len() as u8);
        out.extend_from_slice(&self.writer_id);
        out
    }

    /// Reads a position from the bytes of [`Position::to_bytes`]. Bytes of another shape fail
    /// with [`ErrorKind::CorruptData`].
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        let malformed = || Error::new(ErrorKind::CorruptData, "the bytes are not a position");
        let (&format, rest) = bytes.split_first().ok_or_else(malformed)?;
        if format != POSITION_FORMAT || rest.len() < 41 {
            return Err(malformed());
        }
        let (database_id, rest) = rest.split_at(16);
        let mut database = [0; 16];
        database.copy_from_slice(database_id);
        let (term, rest) = rest.split_at(8);
        let (lsn, rest) = rest.split_at(8);
        let (chain, rest) = rest.split_at(8);
        let (&length, writer_id) = rest.split_first().ok_or_else(malformed)?;
        if writer_id.len() != usize::from(length) {
            return Err(malformed());
        }
        check_epoch(le_u64(term), writer_id).map_err(|_| malformed())?;
        Ok(Self {
            term: le_u64(term),
            writer_id: writer_id.to_vec(),
            lsn: le_u64(lsn),
            chain: le_u64(chain),
            database_id: u128::from_le_bytes(database),
        })
    }
}

/// What a database does with local writes, from [`Database::role`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Role {
    /// The database takes local writes. A database that never took part in replication is a
    /// writer.
    Writer,
    /// The database follows another writer: it refuses local writes and applies changes.
    Replica,
    /// The database was the writer of its newest term but takes no write and applies no change
    /// until [`Database::promote`] makes it the writer again or [`Database::demote`] makes it a
    /// replica. A writer opens this way after every restart, and a writer that hears of a higher
    /// term from a peer becomes fenced.
    Fenced,
}

/// One term of a database's history, from [`Database::history`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct HistoryEntry {
    term: u64,
    writer_id: Vec<u8>,
    start_lsn: u64,
    end_lsn: Option<u64>,
    incarnation: u128,
}

impl HistoryEntry {
    /// The term.
    pub fn term(&self) -> u64 {
        self.term
    }

    /// The id of the writer that held the term.
    pub fn writer_id(&self) -> &[u8] {
        &self.writer_id
    }

    /// The LSN of the change that began the term.
    pub fn start_lsn(&self) -> u64 {
        self.start_lsn
    }

    /// The LSN of the change where the writer stepped down, if it did.
    pub fn end_lsn(&self) -> Option<u64> {
        self.end_lsn
    }

    /// The random id of the database file that began the term, which tells apart two files that
    /// began the same term under the same writer id at the same LSN.
    pub fn incarnation(&self) -> u128 {
        self.incarnation
    }
}

/// Every term a database has followed or been the writer of, oldest first, from
/// [`Database::history`]. A host sends it to another node as the bytes of [`History::to_bytes`]
/// for [`Database::compare_lineage`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct History {
    entries: Vec<HistoryEntry>,
}

impl History {
    /// The terms, oldest first.
    pub fn entries(&self) -> &[HistoryEntry] {
        &self.entries
    }

    /// The history as bytes that [`History::from_bytes`] reads back.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = vec![HISTORY_FORMAT];
        out.extend_from_slice(&(self.entries.len() as u32).to_le_bytes());
        for entry in &self.entries {
            out.extend_from_slice(&entry.term.to_le_bytes());
            out.extend_from_slice(&entry.start_lsn.to_le_bytes());
            out.extend_from_slice(&entry.end_lsn.unwrap_or_default().to_le_bytes());
            out.extend_from_slice(&entry.incarnation.to_le_bytes());
            out.push(entry.writer_id.len() as u8);
            out.extend_from_slice(&entry.writer_id);
        }
        out
    }

    /// Reads a history from the bytes of [`History::to_bytes`]. Bytes of another shape, or terms
    /// out of order, fail with [`ErrorKind::CorruptData`].
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        let malformed = || Error::new(ErrorKind::CorruptData, "the bytes are not a history");
        let (&format, rest) = bytes.split_first().ok_or_else(malformed)?;
        if format != HISTORY_FORMAT || rest.len() < 4 {
            return Err(malformed());
        }
        let (count, mut rest) = rest.split_at(4);
        let count = u32::from_le_bytes([count[0], count[1], count[2], count[3]]) as usize;
        let mut entries: Vec<HistoryEntry> = Vec::with_capacity(count.min(rest.len() / 41));
        for _ in 0..count {
            if rest.len() < 41 {
                return Err(malformed());
            }
            let (fixed, after) = rest.split_at(41);
            let length = usize::from(fixed[40]);
            if after.len() < length {
                return Err(malformed());
            }
            let (writer_id, after) = after.split_at(length);
            let end = le_u64(&fixed[16..24]);
            let entry = HistoryEntry {
                term: le_u64(&fixed[..8]),
                writer_id: writer_id.to_vec(),
                start_lsn: le_u64(&fixed[8..16]),
                end_lsn: (end != 0).then_some(end),
                incarnation: u128::from_le_bytes(
                    fixed[24..40].try_into().map_err(|_| malformed())?,
                ),
            };
            let ordered = entries
                .last()
                .is_none_or(|last| last.term < entry.term && last.start_lsn < entry.start_lsn);
            if entry.term == 0 || check_epoch(entry.term, writer_id).is_err() || !ordered {
                return Err(malformed());
            }
            entries.push(entry);
            rest = after;
        }
        if !rest.is_empty() {
            return Err(malformed());
        }
        Ok(Self { entries })
    }
}

/// How the history of another database relates to this one's, from
/// [`Database::compare_lineage`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Lineage {
    /// Both databases hold the same changes.
    Same,
    /// This database holds a prefix of the other's changes, and can apply the rest from it.
    Behind,
    /// The other database holds a prefix of this one's changes, and can apply the rest from it.
    Ahead,
    /// The histories hold different changes from `fork_lsn` on, so one side must install a
    /// snapshot of the other.
    #[non_exhaustive]
    Diverged {
        /// The first LSN at which the histories can differ.
        fork_lsn: u64,
    },
    /// The two databases follow the histories of different databases, so neither can catch up
    /// from the other or install a snapshot of it in place of its own data.
    DifferentDatabase,
}

/// The facts behind a replication error, from [`Error::replication_failure`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ReplicationFailure {
    /// A change or a promotion carried a term lower than `seen_term`, the highest term this
    /// database has seen.
    #[non_exhaustive]
    Fenced {
        /// The highest term this database has seen.
        seen_term: u64,
    },
    /// This database, at `local`, holds other changes than the database at `remote` from
    /// `fork_lsn` on, as far as this database can tell.
    #[non_exhaustive]
    Diverged {
        /// This database's position when it found the divergence.
        local: Position,
        /// The other database's position, or the change it offered.
        remote: Position,
        /// The first LSN at which the histories can differ.
        fork_lsn: u64,
    },
    /// The log no longer holds the changes asked for, and the oldest it holds is `oldest_lsn`.
    #[non_exhaustive]
    TooFarBehind {
        /// The oldest LSN the log still holds.
        oldest_lsn: u64,
    },
    /// The changes do not reach this database's last change, and the next one it needs is
    /// `expected_lsn`.
    #[non_exhaustive]
    Gap {
        /// The LSN of the next change this database needs.
        expected_lsn: u64,
    },
    /// This database follows the history of database `local`, and the other side belongs to
    /// database `remote`.
    #[non_exhaustive]
    DifferentDatabase {
        /// The database this one follows.
        local: u128,
        /// The database the other side belongs to.
        remote: u128,
    },
}

impl Error {
    /// The facts behind an error of kind [`ErrorKind::Fenced`], [`ErrorKind::Diverged`],
    /// [`ErrorKind::TooFarBehind`], [`ErrorKind::Gap`], or [`ErrorKind::DifferentDatabase`],
    /// and `None` for every other error.
    pub fn replication_failure(&self) -> Option<ReplicationFailure> {
        let storage = self.storage()?;
        let engine = std::error::Error::source(storage)?.downcast_ref::<EngineError>()?;
        let EngineError::Replication(refusal) = engine else {
            return None;
        };
        Some(match refusal.failure.clone()? {
            Failure::Fenced { seen_term } => ReplicationFailure::Fenced { seen_term },
            Failure::Diverged {
                database,
                local,
                remote,
                fork_lsn,
            } => {
                let at = |lsn: u64| if lsn == 0 { 0 } else { database };
                ReplicationFailure::Diverged {
                    local: Position::of(local.0, local.1, local.2).in_database(at(local.1)),
                    remote: Position::of(remote.0, remote.1, remote.2).in_database(at(remote.1)),
                    fork_lsn,
                }
            }
            Failure::TooFarBehind { oldest_lsn } => ReplicationFailure::TooFarBehind { oldest_lsn },
            Failure::Gap { expected_lsn } => ReplicationFailure::Gap { expected_lsn },
            Failure::DifferentDatabase { local, remote } => {
                ReplicationFailure::DifferentDatabase { local, remote }
            }
        })
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
/// writer.promote(1, b"node-a")?;
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
        let database = |lsn: u64| if lsn == 0 { 0 } else { decoded.database };
        let after = Position::of(
            decoded.after.clone(),
            decoded.after_lsn,
            decoded.after_chain,
        )
        .in_database(database(decoded.after_lsn));
        let last = Position::of(decoded.last.clone(), decoded.last_lsn(), decoded.last_chain)
            .in_database(database(decoded.last_lsn()));
        Ok(Self {
            bytes,
            decoded,
            after,
            last,
        })
    }

    /// Reads changes from the bytes of [`Changes::as_bytes`], which may come from an untrusted
    /// peer. Bytes that fail a checksum, do not decode, or break the chain of changes fail with
    /// [`ErrorKind::CorruptData`].
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
    /// began or stepped down from a term.
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

fn diverged(local: Position, remote: &Position, fork_lsn: u64) -> Error {
    let refusal = crate::engine::error::Refusal {
        kind: ErrorKind::Diverged,
        message: format!(
            "LSN {} of term {} is not part of this database's history, which holds other changes \
             from LSN {fork_lsn} onward",
            remote.lsn, remote.term
        ),
        failure: Some(Failure::Diverged {
            database: local.database_id.max(remote.database_id),
            local: (local.epoch(), local.lsn, local.chain),
            remote: (remote.epoch(), remote.lsn, remote.chain),
            fork_lsn,
        }),
    };
    Error::from(StorageError::from(EngineError::Replication(Box::new(
        refusal,
    ))))
}

fn engine_error(error: EngineError) -> Error {
    Error::from(StorageError::from(error))
}

impl Database {
    fn roles(&self) -> MutexGuard<'_, ()> {
        self.roles.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The position of the last change this database holds, which every read that begins now
    /// sees. A host that must not lose a commit waits until a replica reports a position at or
    /// past the writer's position right after that commit.
    pub fn position(&self) -> Position {
        let engine = self.graph.storage();
        let (epoch, lsn, chain) = engine.position();
        Position::of(epoch, lsn, chain).in_database(engine.lineage())
    }

    /// What this database does with local writes.
    pub fn role(&self) -> Role {
        match self.graph.storage().role() {
            EngineRole::Writer => Role::Writer,
            EngineRole::Replica => Role::Replica,
            EngineRole::Fenced => Role::Fenced,
        }
    }

    /// Every term this database has followed or been the writer of, oldest first.
    pub fn history(&self) -> History {
        History {
            entries: self
                .graph
                .storage()
                .epochs()
                .into_iter()
                .map(|epoch| HistoryEntry {
                    term: epoch.term,
                    writer_id: epoch.writer_id,
                    start_lsn: epoch.start_lsn,
                    end_lsn: (epoch.ended_lsn != 0).then_some(epoch.ended_lsn),
                    incarnation: epoch.incarnation,
                })
                .collect(),
        }
    }

    /// How the history of a database at `remote` whose terms are `remote_history` relates to
    /// this one's, so a host can tell which side applies changes from the other and which must
    /// install a snapshot.
    pub fn compare_lineage(&self, remote_history: &History, remote: &Position) -> Lineage {
        let local = self.position();
        if local.database_id != 0
            && remote.database_id != 0
            && local.database_id != remote.database_id
        {
            return Lineage::DifferentDatabase;
        }
        let mine = self.history();
        let common = mine
            .entries
            .iter()
            .zip(&remote_history.entries)
            .take_while(|(left, right)| {
                (left.term, &left.writer_id, left.start_lsn, left.incarnation)
                    == (
                        right.term,
                        &right.writer_id,
                        right.start_lsn,
                        right.incarnation,
                    )
            })
            .count();
        let fork = match (mine.entries.get(common), remote_history.entries.get(common)) {
            (Some(left), Some(right)) => left.start_lsn.min(right.start_lsn),
            (Some(only), None) | (None, Some(only)) => only.start_lsn,
            (None, None) => u64::MAX,
        };
        if local.lsn >= fork && remote.lsn >= fork {
            return Lineage::Diverged { fork_lsn: fork };
        }
        if local.lsn == remote.lsn {
            return if local.chain == remote.chain {
                Lineage::Same
            } else {
                Lineage::Diverged {
                    fork_lsn: local.lsn.max(1),
                }
            };
        }
        if local.lsn < remote.lsn {
            Lineage::Behind
        } else {
            Lineage::Ahead
        }
    }

    /// The whole transactions after `position`, which another database, writer or replica,
    /// reported as its own, up to about `max_bytes` and at least one when there is any. A
    /// replica serves changes too, so they can be relayed from one replica to the next.
    ///
    /// When `position` is this database's own or lies past it in the same term, the changes
    /// are empty. A `position` that is not part of this database's history, by its term, its
    /// writer, or its chain hash, fails with [`ErrorKind::Diverged`], and one whose following
    /// changes the log no longer keeps fails with [`ErrorKind::TooFarBehind`], after which the
    /// database behind it installs a snapshot.
    ///
    /// A `position` of a term higher than any this database has seen makes a writer
    /// [fenced](Role::Fenced), so a host passes only positions of authenticated peers.
    pub fn changes_since(&self, position: &Position, max_bytes: usize) -> Result<Changes, Error> {
        let engine = self.graph.storage();
        let local = self.position();
        if position.database_id != 0
            && local.database_id != 0
            && position.database_id != local.database_id
        {
            return Err(engine_error(crate::engine::different(
                local.database_id,
                position.database_id,
            )));
        }
        engine.observe_term(position.term).map_err(engine_error)?;
        let local = self.position();
        let epochs = engine.epochs();
        let asked = position.epoch();
        let records = if position.lsn <= local.lsn {
            let held = epoch_at(&epochs, position.lsn);
            let chain_differs = position.lsn == local.lsn && position.chain != local.chain;
            if position.lsn > 0 && (held != asked || chain_differs) {
                let fork = crate::engine::fork_lsn(&epochs, &asked, position.lsn);
                return Err(diverged(local, position, fork));
            }
            engine
                .read_changes(position.lsn, position.chain, max_bytes)
                .map_err(engine_error)?
        } else {
            let newest = top(&epochs);
            if asked != newest && asked.term <= newest.term {
                let fork = crate::engine::fork_lsn(&epochs, &asked, position.lsn);
                return Err(diverged(local, position, fork));
            }
            Vec::new()
        };
        let bytes = changes::encode(
            local.database_id,
            &asked,
            position.lsn,
            position.chain,
            &records,
        )
        .map_err(engine_error)?;
        Changes::decode(bytes)
    }

    /// Applies `changes` of another database to this replica and returns the position it
    /// reached. Each change is logged and made durable before readers see it, as a commit is.
    /// Changes this replica already holds in the same history are skipped, so applying the same
    /// changes twice is safe.
    ///
    /// A replica that holds no change takes the database id of the first changes it applies,
    /// and changes of another database fail with [`ErrorKind::DifferentDatabase`].
    ///
    /// It fails with [`ErrorKind::NotReplica`] on a writer or a fenced writer, with
    /// [`ErrorKind::Gap`] when the changes start after the last change held, with
    /// [`ErrorKind::Fenced`] when they come from a term older than the newest this database has
    /// seen, with [`ErrorKind::Diverged`] when this database holds other changes in their
    /// place, and with [`ErrorKind::UnsupportedFormatVersion`] at a change that records a graph
    /// format newer than this build reads. A failure leaves every change before it applied.
    pub fn apply(&self, changes: &Changes) -> Result<Position, Error> {
        let _roles = self.roles();
        let unreadable = first_metadata_put(&changes.decoded, newer_format_put);
        let readable = match unreadable {
            Some((index, _)) => &Decoded {
                records: changes.decoded.records[..index].to_vec(),
                ..changes.decoded.clone()
            },
            None => &changes.decoded,
        };
        if let Some(found) = first_metadata_put(readable, older_format_put) {
            self.graph.wait_for_upgrade(found.1);
        }
        let indexes = readable.touches(TableId::of(Table::Indexes));
        self.graph
            .apply_outside(indexes, |engine| engine.apply_changes(readable))?
            .map_err(engine_error)?;
        if let Some((_, version)) = unreadable {
            return Err(Error::from(GraphError::UnsupportedFormatVersion(version)));
        }
        Ok(self.position())
    }

    /// Makes this database the writer of `term` under `writer_id` and returns its position. The
    /// host gives each promotion a term higher than every term the database has seen and a
    /// writer id of 1 to 64 bytes that names the node. A promotion records the new term as a
    /// change of its own, which replicas apply as they apply commits, so every database that
    /// follows it refuses changes of older terms with [`ErrorKind::Fenced`].
    ///
    /// Promoting a [fenced](Role::Fenced) writer again with its own newest term and writer id
    /// records nothing and makes it the writer again, as a writer that restarted needs. That
    /// works only on the file that began the term, never on a copy of it or after a demotion,
    /// which fail with [`ErrorKind::Diverged`]. A lower term fails with `Fenced`, and the newest
    /// term under another writer id fails with `Diverged`. A replica that never applied a
    /// change also records the graph's format, as opening a new database as a writer does.
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
            .map_err(engine_error)?;
        self.graph.set_replica(false);
        self.graph.finish_pending_upgrade(true)?;
        self.graph.record_format_if_missing()?;
        Ok(self.position())
    }

    /// Makes this writer a replica and returns its final position. New writes, and the commit
    /// of a write transaction already open, fail with [`ErrorKind::NotWriter`] at once. It then
    /// waits up to `wait` for the open write transaction to end and records that the writer
    /// stepped down, so the file opens as a replica from then on. When the wait runs out it
    /// fails with [`ErrorKind::WriterBusy`] and the database stays [fenced](Role::Fenced), and
    /// calling it again once the transaction ended finishes the demotion.
    ///
    /// The next writer applies the changes up to the final position and promotes with a higher
    /// term, so a planned handover loses no write.
    ///
    /// A writer that lost its lease without a handover, and has heard of the new writer's term
    /// from [`Database::changes_since`] or [`Database::acknowledge`] with the new writer's
    /// position, steps aside instead: it records no step down, which would conflict with the
    /// new writer's first change, and opens as a replica from then on. It then applies the new
    /// writer's changes when it holds none the new writer lacks, and fails with
    /// [`ErrorKind::Diverged`] when it does, as any replica would.
    pub fn demote(&self, wait: Duration) -> Result<Position, Error> {
        let _roles = self.roles();
        self.graph
            .storage()
            .writable("step down")
            .map_err(engine_error)?;
        self.graph.set_replica(true);
        self.graph
            .maintain(|engine| engine.demote(wait))?
            .map_err(engine_error)?;
        Ok(self.position())
    }
}

/// Damages a batch of valid changes by `bytes`, reseals its checksums, decodes it, applies it to
/// a replica, and reads the replica, for the fuzzer. Fails when decoded changes encode to other
/// bytes. A panic is the failure the fuzzer looks for.
#[cfg(any(test, fuzzing))]
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

fn first_metadata_put(
    decoded: &Decoded,
    matches: fn(&[u8], &[u8]) -> Option<u32>,
) -> Option<(usize, u32)> {
    let metadata = TableId::of(Table::Metadata);
    decoded
        .records
        .iter()
        .enumerate()
        .find_map(|(index, change)| match &change.record.body {
            Body::Batch(ops) => ops
                .iter()
                .find_map(|op| match op {
                    Op::Put { table, key, value } if *table == metadata => matches(key, value),
                    _ => None,
                })
                .map(|version| (index, version)),
            _ => None,
        })
}

#[cfg(test)]
#[path = "replication.test.rs"]
mod tests;

#[cfg(test)]
#[path = "replication_sim.test.rs"]
mod sim_tests;
