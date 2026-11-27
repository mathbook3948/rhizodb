use std::io;
use std::ops::Bound;
use std::sync::Arc;
use std::sync::TryLockError;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use super::Engine;
use super::Writer;
use super::hybrid_clock_after;
use super::lock;
use super::stage_record;
use crate::engine::block_file::BlockFile;
use crate::engine::block_file::BlockSize;
use crate::engine::block_file::FileSystem;
use crate::engine::cache::Access;
use crate::engine::changes::ChangeRecord;
use crate::engine::changes::Decoded;
use crate::engine::changes::EpochId;
use crate::engine::changes::reseal;
use crate::engine::error::EngineError;
use crate::engine::error::Failure;
use crate::engine::error::Refusal;
use crate::engine::header::Header;
use crate::engine::header::Replication;
use crate::engine::layout::TableId;
use crate::engine::layout::WAL_BLOCK;
use crate::engine::node::CellValue;
use crate::engine::staged::Staged;
use crate::engine::view::Layers;
use crate::engine::view::View;
use crate::engine::wal::segment_files;
use crate::engine::wal::segment_name;
use crate::engine::wal_format::Body;
use crate::engine::wal_format::NotRecord;
use crate::engine::wal_format::RECORD_HEADER;
use crate::engine::wal_format::RECORD_LIMIT;
use crate::engine::wal_format::SegmentHeader;
use crate::engine::wal_format::Step;
use crate::engine::wal_format::chain_after;
use crate::engine::wal_format::config_record;
use crate::engine::wal_format::decode_record;
use crate::engine::wal_format::padded;
use crate::error::ErrorKind;
use crate::storage::StorageError;
use crate::storage::Table;

/// A term of the database's history: the writer and incarnation that held it, the LSN of the
/// configuration record that began it, and the LSN of the one that ended it, or zero.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Epoch {
    pub(crate) term: u64,
    pub(crate) writer_id: Vec<u8>,
    pub(crate) start_lsn: u64,
    pub(crate) incarnation: u128,
    pub(crate) ended_lsn: u64,
}

impl Epoch {
    fn id(&self) -> EpochId {
        EpochId {
            term: self.term,
            writer_id: self.writer_id.clone(),
        }
    }
}

/// What a database does with writes: takes them, follows a writer, or neither until the host
/// promotes it again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Role {
    Writer,
    Replica,
    Fenced,
}

fn history_table() -> TableId {
    TableId::of(Table::ReplicationHistory)
}

/// The key and value under which the history table keeps `epoch`.
pub(super) fn history_entry(epoch: &Epoch) -> ([u8; 8], Vec<u8>) {
    let mut value = Vec::with_capacity(32 + epoch.writer_id.len());
    value.extend_from_slice(&epoch.start_lsn.to_le_bytes());
    value.extend_from_slice(&epoch.ended_lsn.to_le_bytes());
    value.extend_from_slice(&epoch.incarnation.to_le_bytes());
    value.extend_from_slice(&epoch.writer_id);
    (epoch.term.to_be_bytes(), value)
}

fn le_u64(bytes: &[u8]) -> u64 {
    let mut array = [0; 8];
    array.copy_from_slice(bytes);
    u64::from_le_bytes(array)
}

pub(super) fn decode_epoch(key: &[u8], value: &[u8]) -> Result<Epoch, EngineError> {
    let damaged = || EngineError::corrupt("an entry of the replication history is damaged");
    let term = <[u8; 8]>::try_from(key).map_err(|_| damaged())?;
    if value.len() < 33 {
        return Err(damaged());
    }
    let mut incarnation = [0; 16];
    incarnation.copy_from_slice(&value[16..32]);
    Ok(Epoch {
        term: u64::from_be_bytes(term),
        start_lsn: le_u64(&value[..8]),
        ended_lsn: le_u64(&value[8..16]),
        incarnation: u128::from_le_bytes(incarnation),
        writer_id: value[32..].to_vec(),
    })
}

/// Every term the layers hold, in order, which fails when their LSNs do not grow with them.
pub(crate) fn load_epochs(layers: &Arc<Layers>) -> Result<Vec<Epoch>, EngineError> {
    epochs_in(&View::new(u64::MAX, Arc::clone(layers)))
}

/// Every term `view` holds, in order.
pub(crate) fn epochs_in(view: &View) -> Result<Vec<Epoch>, EngineError> {
    let mut epochs: Vec<Epoch> = Vec::new();
    view.scan(
        history_table(),
        Bound::Unbounded,
        Bound::Unbounded,
        &mut |key, value| {
            let epoch = match value {
                CellValue::Inline(value) => decode_epoch(key, value),
                CellValue::External(blob) => decode_epoch(key, &view.load(&blob, Access::Point)?),
            }
            .map_err(StorageError::from)?;
            if epochs
                .last()
                .is_some_and(|last| last.start_lsn >= epoch.start_lsn || last.term >= epoch.term)
            {
                return Err(StorageError::from(EngineError::corrupt(
                    "the replication history is out of order",
                )));
            }
            epochs.push(epoch);
            Ok::<(), StorageError>(())
        },
    )?;
    Ok(epochs)
}

/// The epoch that made the record at `lsn`: the last term begun at or before it, or term 0.
pub(crate) fn epoch_at(epochs: &[Epoch], lsn: u64) -> EpochId {
    epochs
        .iter()
        .rev()
        .find(|epoch| lsn > 0 && epoch.start_lsn <= lsn)
        .map(Epoch::id)
        .unwrap_or_default()
}

fn top(epochs: &[Epoch]) -> EpochId {
    epochs.last().map(Epoch::id).unwrap_or_default()
}

/// The first LSN at which a history that holds `remote` at `lsn` can differ from `epochs`.
pub(crate) fn fork_lsn(epochs: &[Epoch], remote: &EpochId, lsn: u64) -> u64 {
    let local = epochs
        .iter()
        .position(|epoch| epoch.term == remote.term && epoch.writer_id == remote.writer_id);
    let fork = match local {
        Some(index) => epochs.get(index + 1).map(|next| next.start_lsn),
        None => epochs
            .iter()
            .find(|epoch| epoch.term > remote.term)
            .map(|epoch| epoch.start_lsn),
    };
    fork.unwrap_or(lsn).min(lsn).max(1)
}

/// The role a file opens in: a writer that began the newest term and never stepped down opens
/// fenced until the host promotes it again, unless it stepped aside for a higher term it heard
/// of, and a file with no history follows its flag.
pub(super) fn role_at_open(header: &Header, epochs: &[Epoch]) -> Role {
    let replication = &header.replication;
    match epochs.last() {
        None if replication.replica => Role::Replica,
        None => Role::Writer,
        Some(top)
            if top.incarnation == replication.incarnation
                && top.ended_lsn == 0
                && !stepped_aside(replication, top) =>
        {
            Role::Fenced
        }
        Some(_) => Role::Replica,
    }
}

fn stepped_aside(replication: &Replication, top: &Epoch) -> bool {
    replication.replica && replication.seen_term > top.term
}

fn refuse(kind: ErrorKind, message: String, failure: Option<Failure>) -> EngineError {
    EngineError::Replication(Box::new(Refusal {
        kind,
        message,
        failure,
    }))
}

pub(crate) fn different(local: u128, remote: u128) -> EngineError {
    refuse(
        ErrorKind::DifferentDatabase,
        format!("database {local:#x} cannot follow the changes of database {remote:#x}"),
        Some(Failure::DifferentDatabase { local, remote }),
    )
}

pub(super) fn not_writer() -> EngineError {
    refuse(
        ErrorKind::NotWriter,
        "this database is not the writer, so it takes no local write".to_owned(),
        None,
    )
}

pub(crate) fn fenced(term: u64, seen_term: u64) -> EngineError {
    refuse(
        ErrorKind::Fenced,
        format!("term {term} is older than term {seen_term}, which this database has seen"),
        Some(Failure::Fenced { seen_term }),
    )
}

fn diverged(
    database: u128,
    local: (EpochId, u64, u64),
    remote: (EpochId, u64, u64),
    fork_lsn: u64,
) -> EngineError {
    refuse(
        ErrorKind::Diverged,
        format!(
            "LSN {} of term {} is not part of this database's history, which differs from LSN \
             {fork_lsn} on",
            remote.1, remote.0.term
        ),
        Some(Failure::Diverged {
            database,
            local,
            remote,
            fork_lsn,
        }),
    )
}

impl<F: FileSystem + 'static> Engine<F> {
    /// Every term of the history, oldest first.
    pub(crate) fn epochs(&self) -> Vec<Epoch> {
        lock(&self.shared.epochs).clone()
    }

    pub(crate) fn role(&self) -> Role {
        *lock(&self.shared.role)
    }

    /// The highest term this database has seen, in its history or from a peer.
    pub(crate) fn seen_term(&self) -> u64 {
        let seen = lock(&self.shared.identity).replication.seen_term;
        seen.max(top(&lock(&self.shared.epochs)).term)
    }

    /// The epoch, LSN, and chain hash of the last change readers see.
    pub(crate) fn position(&self) -> (EpochId, u64, u64) {
        let (lsn, chain) = {
            let published = lock(&self.shared.published);
            (published.view.snapshot, published.chain)
        };
        (epoch_at(&lock(&self.shared.epochs), lsn), lsn, chain)
    }

    /// The id of the database whose history this file holds: the one it took from its first
    /// applied change or installed copy, its own while it is a writer that never took one, and
    /// zero while it holds nothing.
    pub(crate) fn lineage(&self) -> u128 {
        let (origin, own) = {
            let identity = *lock(&self.shared.identity);
            (identity.replication.origin, identity.database_id)
        };
        if lock(&self.shared.published).view.snapshot == 0 {
            0
        } else if origin != 0 {
            origin
        } else {
            own
        }
    }

    fn set_role(&self, role: Role) {
        *lock(&self.shared.role) = role;
        self.shared
            .accepting
            .store(role == Role::Writer, Ordering::Release);
    }

    fn write_replication(&self, change: impl FnOnce(&mut Replication)) -> Result<(), EngineError> {
        let shared = &*self.shared;
        let mut durable = lock(&shared.durable);
        let mut replication = durable.header.replication;
        change(&mut replication);
        let header = Header {
            generation: durable.header.generation + 1,
            replication,
            ..durable.header
        };
        if let Err(error) = shared
            .pages
            .write_header(&header, &shared.retained_area(&header))
        {
            shared.poison_with(&error);
            return Err(error);
        }
        lock(&shared.identity).replication = header.replication;
        durable.header = header;
        Ok(())
    }

    /// Records that a peer has seen `term`. A term above every one this database has seen is
    /// kept, and a writer that sees one stops taking writes until the host promotes it with a
    /// higher term still.
    pub(crate) fn observe_term(&self, term: u64) -> Result<(), EngineError> {
        if term <= self.seen_term() {
            return Ok(());
        }
        self.write_replication(|replication| {
            replication.seen_term = replication.seen_term.max(term);
        })?;
        let mut role = lock(&self.shared.role);
        if *role == Role::Writer {
            *role = Role::Fenced;
            self.shared.accepting.store(false, Ordering::Release);
        }
        Ok(())
    }

    /// Makes a file that holds no change and no history a replica, and refuses a file that a
    /// writer holds.
    pub(crate) fn declare_replica(&self) -> Result<(), EngineError> {
        let writer = lock(&self.shared.writer);
        match self.role() {
            Role::Replica => Ok(()),
            Role::Writer if writer.last_lsn == 0 && self.epochs().is_empty() => {
                self.write_replication(|replication| replication.replica = true)?;
                self.set_role(Role::Replica);
                Ok(())
            }
            Role::Writer | Role::Fenced => Err(refuse(
                ErrorKind::NotReplica,
                "the file holds a writer's changes, so it opens as a replica only after a demotion"
                    .to_owned(),
                None,
            )),
        }
    }

    /// The records after `after` that readers see, from the log segments still on disk, sealed
    /// for a batch of changes. They stop at the first record past `max_bytes`, but hold at least
    /// one record when there is any. Fails as diverged when the first record does not follow
    /// the chain hash `after_chain`.
    pub(crate) fn read_changes(
        &self,
        after: u64,
        after_chain: u64,
        max_bytes: usize,
    ) -> Result<Vec<Vec<u8>>, EngineError> {
        let shared = &*self.shared;
        let upto = lock(&shared.published).view.snapshot;
        if after >= upto {
            return Ok(Vec::new());
        }
        let database_id = lock(&shared.identity).database_id;
        let fs = &*shared.fs;
        let sequences = segment_files(fs, &shared.name)?;
        let mut start = None;
        let mut oldest = upto + 1;
        for (index, sequence) in sequences.iter().enumerate().rev() {
            let newest = index + 1 == sequences.len();
            let segment = match open_segment(fs, &shared.name, *sequence, database_id) {
                Ok(segment) => segment,
                Err(_) if newest => continue,
                Err(error) if is_gone(&error) => break,
                Err(error) => return Err(error),
            };
            oldest = segment.1.first_lsn;
            if segment.1.first_lsn <= after + 1 {
                start = Some((index, segment));
                break;
            }
        }
        let Some((first, segment)) = start else {
            return Err(too_far_behind(after, oldest));
        };
        let mut records = Vec::new();
        let mut bytes = 0;
        let mut segment = Some(segment);
        for index in first..sequences.len() {
            let (file, header) = match segment.take() {
                Some(segment) => segment,
                None => match open_segment(fs, &shared.name, sequences[index], database_id) {
                    Ok(segment) => segment,
                    Err(_) if index + 1 == sequences.len() => break,
                    Err(error) if is_gone(&error) && records.is_empty() => {
                        return Err(too_far_behind(after, oldest));
                    }
                    Err(error) if is_gone(&error) => break,
                    Err(error) => return Err(error),
                },
            };
            let done = read_segment(&file, &header, after, upto, &mut |record, prev| {
                if records.is_empty() && prev.is_some_and(|prev| prev != after_chain) {
                    let epochs = lock(&shared.epochs);
                    let held = epoch_at(&epochs, after);
                    return Err(diverged(
                        self.lineage(),
                        (held.clone(), after, prev.unwrap_or_default()),
                        (held, after, after_chain),
                        after,
                    ));
                }
                if !records.is_empty() && bytes + record.len() > max_bytes {
                    return Ok(true);
                }
                bytes += record.len();
                records.push(reseal(record)?);
                Ok(bytes >= max_bytes)
            })?;
            if done {
                break;
            }
        }
        if records.is_empty() {
            return Err(too_far_behind(after, oldest));
        }
        Ok(records)
    }

    /// Applies `changes` to a replica in order: logs each record that follows the last one held,
    /// under the writer lock, and skips each one already held in the same epoch. Stops at the
    /// first record it refuses, with the earlier ones applied.
    pub(crate) fn apply_changes(&self, changes: &Decoded) -> Result<(), EngineError> {
        let shared = &*self.shared;
        let mut writer = lock(&shared.writer);
        if shared.is_poisoned() {
            return Err(shared.refusal());
        }
        if self.role() != Role::Replica {
            return Err(refuse(
                ErrorKind::NotReplica,
                "a writer takes no changes of another writer until it is demoted".to_owned(),
                None,
            ));
        }
        if changes.after_lsn > writer.last_lsn {
            return Err(refuse(
                ErrorKind::Gap,
                format!(
                    "the changes follow LSN {}, but the database holds changes up to LSN {}",
                    changes.after_lsn, writer.last_lsn
                ),
                Some(Failure::Gap {
                    expected_lsn: writer.last_lsn + 1,
                }),
            ));
        }
        if !changes.records.is_empty() && changes.database != 0 {
            match self.lineage() {
                0 => self.write_replication(|replication| replication.origin = changes.database)?,
                local if local != changes.database => {
                    return Err(different(local, changes.database));
                }
                _ => {}
            }
        }
        let seen = lock(&shared.identity).replication.seen_term;
        let mut offered = changes.after.clone();
        let mut offered_chain = changes.after_chain;
        {
            let epochs = lock(&shared.epochs);
            let held = epoch_at(&epochs, changes.after_lsn);
            let chain_differs =
                changes.after_lsn == writer.last_lsn && changes.after_chain != writer.chain;
            if changes.after_lsn > 0 && (held != offered || chain_differs) {
                return Err(self.conflict(
                    &epochs,
                    &writer,
                    seen,
                    (offered, changes.after_lsn, changes.after_chain),
                ));
            }
        }
        for change in &changes.records {
            let record = &change.record;
            if let Body::Config {
                writer_id,
                step: Step::Begin,
                ..
            } = &record.body
            {
                offered = EpochId {
                    term: record.term,
                    writer_id: writer_id.clone(),
                };
            }
            let chain = chain_after(offered_chain, &change.bytes);
            let epochs = lock(&shared.epochs).clone();
            let held = epoch_at(&epochs, record.lsn);
            let newest = top(&epochs);
            if record.lsn < writer.last_lsn {
                if held != offered {
                    return Err(self.conflict(
                        &epochs,
                        &writer,
                        seen,
                        (offered, record.lsn, chain),
                    ));
                }
                offered_chain = chain;
                continue;
            }
            if record.lsn == writer.last_lsn {
                if held != offered || chain != writer.chain {
                    return Err(self.conflict(
                        &epochs,
                        &writer,
                        seen,
                        (offered, record.lsn, chain),
                    ));
                }
                offered_chain = chain;
                continue;
            }
            let begins = matches!(
                record.body,
                Body::Config {
                    step: Step::Begin,
                    ..
                }
            );
            if offered.term < newest.term
                || offered.term < seen
                || (begins && offered.term <= newest.term)
            {
                return Err(self.conflict(&epochs, &writer, seen, (offered, record.lsn, chain)));
            }
            if !begins && offered != newest {
                return Err(self.conflict(&epochs, &writer, seen, (offered, record.lsn, chain)));
            }
            self.apply_record(&mut writer, change)?;
            offered_chain = chain;
        }
        Ok(())
    }

    fn conflict(
        &self,
        epochs: &[Epoch],
        writer: &Writer<F>,
        seen: u64,
        offered: (EpochId, u64, u64),
    ) -> EngineError {
        let newest = top(epochs).term.max(seen);
        if offered.0.term < newest {
            return fenced(offered.0.term, newest);
        }
        let fork = fork_lsn(epochs, &offered.0, offered.1);
        diverged(
            self.lineage(),
            (
                epoch_at(epochs, writer.last_lsn),
                writer.last_lsn,
                writer.chain,
            ),
            offered,
            fork,
        )
    }

    fn apply_record(
        &self,
        writer: &mut Writer<F>,
        change: &ChangeRecord,
    ) -> Result<(), EngineError> {
        let record = &change.record;
        self.drop_segments_aside(writer);
        let view = lock(&self.shared.published).view.clone();
        let mut staged = Staged::new(view);
        stage_record(&mut staged, record.lsn, record.term, record.body.clone())?;
        self.append(writer, change.bytes.clone(), record.lsn, record.hlc)?;
        if let Body::Config { .. } = &record.body {
            self.note_config(writer, record)?;
        }
        self.publish(writer, record.lsn, staged);
        Ok(())
    }

    fn note_config(
        &self,
        writer: &mut Writer<F>,
        record: &crate::engine::wal_format::Record,
    ) -> Result<(), EngineError> {
        let Body::Config {
            writer_id,
            incarnation,
            step,
        } = &record.body
        else {
            return Ok(());
        };
        let mut epochs = lock(&self.shared.epochs);
        match step {
            Step::Begin => {
                writer.term = record.term;
                epochs.push(Epoch {
                    term: record.term,
                    writer_id: writer_id.clone(),
                    start_lsn: record.lsn,
                    incarnation: *incarnation,
                    ended_lsn: 0,
                });
            }
            Step::StepDown => match epochs.last_mut() {
                Some(epoch) if epoch.term == record.term => epoch.ended_lsn = record.lsn,
                _ => {
                    return Err(EngineError::corrupt(
                        "a writer stepped down from a term it never began",
                    ));
                }
            },
        }
        Ok(())
    }

    fn write_config(
        &self,
        writer: &mut Writer<F>,
        term: u64,
        writer_id: &[u8],
        step: Step,
    ) -> Result<(), EngineError> {
        let shared = &*self.shared;
        self.drop_segments_aside(writer);
        let incarnation = lock(&shared.identity).replication.incarnation;
        let lsn = writer.last_lsn + 1;
        let hlc = hybrid_clock_after(writer.last_hlc);
        let bytes = config_record(lsn, term, hlc, writer.chain, (writer_id, incarnation, step))?;
        let record = crate::engine::wal_format::Record {
            lsn,
            term,
            hlc,
            txn: u128::from(lsn),
            prev: Some(writer.chain),
            body: Body::Config {
                writer_id: writer_id.to_vec(),
                incarnation,
                step,
            },
        };
        let mut staged = Staged::new(lock(&shared.published).view.clone());
        stage_record(&mut staged, lsn, term, record.body.clone())?;
        self.append(writer, bytes, lsn, hlc)?;
        self.note_config(writer, &record)?;
        self.publish(writer, lsn, staged);
        Ok(())
    }

    /// Makes this database the writer of `term` under `writer_id`. A term above every one seen
    /// begins with a configuration record. The newest term again, under the same writer id and
    /// begun by this file's incarnation without a step down since, takes writes again with no
    /// record. A lower term fails as fenced and any other reuse of a term as diverged.
    pub(crate) fn promote(&self, term: u64, writer_id: &[u8]) -> Result<(), EngineError> {
        let shared = &*self.shared;
        let mut writer = lock(&shared.writer);
        if shared.is_poisoned() {
            return Err(shared.refusal());
        }
        let epochs = self.epochs();
        let replication = lock(&shared.identity).replication;
        let seen = replication.seen_term.max(top(&epochs).term);
        let offered = EpochId {
            term,
            writer_id: writer_id.to_vec(),
        };
        if let Some(newest) = epochs.last()
            && newest.id() == offered
        {
            if newest.incarnation == replication.incarnation
                && newest.ended_lsn == 0
                && replication.seen_term <= term
            {
                if replication.replica {
                    self.write_replication(|replication| replication.replica = false)?;
                }
                self.set_role(Role::Writer);
                return Ok(());
            }
            return Err(self.conflict(&epochs, &writer, 0, (offered, writer.last_lsn + 1, 0)));
        }
        if term < seen || (term == seen && term > top(&epochs).term) {
            return Err(fenced(term, seen));
        }
        if term == seen {
            return Err(self.conflict(&epochs, &writer, 0, (offered, writer.last_lsn + 1, 0)));
        }
        self.write_config(&mut writer, term, writer_id, Step::Begin)?;
        if replication.replica {
            self.write_replication(|replication| replication.replica = false)?;
        }
        self.set_role(Role::Writer);
        Ok(())
    }

    /// Stops taking writes at once, then waits up to `wait` for the open write transaction to
    /// end and records that the writer stepped down, so the file reopens as a replica. A file
    /// with no history only records that it follows a writer now. When the wait runs out the
    /// database stays fenced, and calling this again finishes the demotion.
    pub(crate) fn demote(&self, wait: Duration) -> Result<(), EngineError> {
        let shared = &*self.shared;
        shared.accepting.store(false, Ordering::Release);
        {
            let mut role = lock(&shared.role);
            if *role == Role::Writer {
                *role = Role::Fenced;
            }
        }
        let deadline = Instant::now().checked_add(wait);
        let mut writer = loop {
            match shared.writer.try_lock() {
                Ok(writer) => break writer,
                Err(TryLockError::Poisoned(poisoned)) => break poisoned.into_inner(),
                Err(TryLockError::WouldBlock)
                    if deadline.is_none_or(|deadline| Instant::now() < deadline) =>
                {
                    thread::sleep(Duration::from_millis(1));
                }
                Err(TryLockError::WouldBlock) => {
                    return Err(refuse(
                        ErrorKind::WriterBusy,
                        "a write transaction stayed open past the demotion's wait".to_owned(),
                        None,
                    ));
                }
            }
        };
        if self.role() == Role::Replica {
            return Ok(());
        }
        if shared.is_poisoned() {
            return Err(shared.refusal());
        }
        let seen = lock(&shared.identity).replication.seen_term;
        match self.epochs().last() {
            Some(newest) if newest.ended_lsn == 0 && seen > newest.term => {
                self.write_replication(|replication| replication.replica = true)?;
            }
            Some(newest) if newest.ended_lsn == 0 => {
                let (term, writer_id) = (newest.term, newest.writer_id.clone());
                self.write_config(&mut writer, term, &writer_id, Step::StepDown)?;
            }
            Some(_) => {}
            None => self.write_replication(|replication| replication.replica = true)?,
        }
        self.set_role(Role::Replica);
        Ok(())
    }
}

fn too_far_behind(after: u64, oldest_lsn: u64) -> EngineError {
    refuse(
        ErrorKind::TooFarBehind,
        format!("the log no longer holds the changes after LSN {after}"),
        Some(Failure::TooFarBehind { oldest_lsn }),
    )
}

fn is_gone(error: &EngineError) -> bool {
    matches!(error, EngineError::Io(io) if io.kind() == io::ErrorKind::NotFound)
}

fn open_segment<F: FileSystem>(
    fs: &F,
    base: &str,
    sequence: u64,
    database_id: u128,
) -> Result<(F::File, SegmentHeader), EngineError> {
    let size = BlockSize::new(WAL_BLOCK).ok_or(EngineError::TooLarge)?;
    let file = fs.open(&segment_name(base, sequence), size)?;
    let mut block = vec![0; WAL_BLOCK];
    if file.block_count()? == 0 {
        return Err(EngineError::corrupt(format!(
            "log segment {sequence} is empty"
        )));
    }
    file.read(0, &mut block)?;
    let header = SegmentHeader::decode(&block)?;
    if header.database_id != database_id || header.sequence != sequence {
        return Err(EngineError::corrupt(format!(
            "log segment {sequence} belongs to another database"
        )));
    }
    Ok((file, header))
}

fn read_segment(
    file: &impl BlockFile,
    header: &SegmentHeader,
    after: u64,
    upto: u64,
    take: &mut dyn FnMut(Vec<u8>, Option<u64>) -> Result<bool, EngineError>,
) -> Result<bool, EngineError> {
    let seed = header.record_seed();
    let blocks = file.block_count()?;
    let unit = header.unit();
    let mut lsn = header.first_lsn;
    let mut at = (unit / WAL_BLOCK) as u64;
    let mut buffer = vec![0; WAL_BLOCK];
    while at < blocks && lsn <= upto {
        buffer.resize(WAL_BLOCK, 0);
        file.read(at, &mut buffer)?;
        let mut head = [0; 4];
        head.copy_from_slice(&buffer[..4]);
        let length = u32::from_le_bytes(head) as usize;
        if !(RECORD_HEADER..=RECORD_LIMIT).contains(&length) {
            break;
        }
        let span = (padded(length, unit) / WAL_BLOCK) as u64;
        if span == 0 || at + span > blocks {
            break;
        }
        buffer.resize(padded(length, unit), 0);
        file.read(at, &mut buffer)?;
        let (record, length) = match decode_record(seed, &buffer) {
            Ok(decoded) => decoded,
            Err(NotRecord::Refused(error)) => return Err(error),
            Err(NotRecord::End | NotRecord::Invalid(_)) => break,
        };
        if record.lsn != lsn {
            return Err(EngineError::corrupt(format!(
                "log segment {} holds LSN {} where {lsn} was next",
                header.sequence, record.lsn
            )));
        }
        if lsn > after {
            buffer.truncate(length);
            if take(std::mem::take(&mut buffer), record.prev)? {
                return Ok(true);
            }
        }
        lsn += 1;
        at += span;
    }
    Ok(lsn > upto)
}
