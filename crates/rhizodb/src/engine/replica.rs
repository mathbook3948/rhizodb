use std::io;
use std::ops::Bound;
use std::sync::Arc;

use super::Engine;
use super::Writer;
use super::hybrid_clock_after;
use super::lock;
use super::stage_record;
use crate::engine::block_file::BlockFile;
use crate::engine::block_file::BlockSize;
use crate::engine::block_file::FileSystem;
use crate::engine::changes::ChangeRecord;
use crate::engine::changes::Decoded;
use crate::engine::changes::EpochId;
use crate::engine::changes::reseal;
use crate::engine::error::EngineError;
use crate::engine::layout::TableId;
use crate::engine::layout::WAL_BLOCK;
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
use crate::engine::wal_format::config_record;
use crate::engine::wal_format::decode_record;
use crate::engine::wal_format::padded;
use crate::error::ErrorKind;
use crate::storage::StorageError;
use crate::storage::Table;

/// A term of the database's history: the writer that held it and the LSN of the configuration
/// record that began it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Epoch {
    pub(crate) term: u64,
    pub(crate) writer_id: Vec<u8>,
    pub(crate) start_lsn: u64,
}

fn history_table() -> TableId {
    TableId::of(Table::ReplicationHistory)
}

/// The key and value under which the history table keeps the term `term`.
pub(super) fn history_entry(term: u64, writer_id: &[u8], lsn: u64) -> ([u8; 8], Vec<u8>) {
    let mut value = Vec::with_capacity(8 + writer_id.len());
    value.extend_from_slice(&lsn.to_le_bytes());
    value.extend_from_slice(writer_id);
    (term.to_be_bytes(), value)
}

fn decode_epoch(key: &[u8], value: &[u8]) -> Result<Epoch, EngineError> {
    let damaged = || EngineError::corrupt("an entry of the replication history is damaged");
    let term = <[u8; 8]>::try_from(key).map_err(|_| damaged())?;
    if value.len() < 9 {
        return Err(damaged());
    }
    let (lsn, writer_id) = value.split_at(8);
    let start = <[u8; 8]>::try_from(lsn).map_err(|_| damaged())?;
    Ok(Epoch {
        term: u64::from_be_bytes(term),
        writer_id: writer_id.to_vec(),
        start_lsn: u64::from_le_bytes(start),
    })
}

/// Every term the layers hold, in order, which fails when their LSNs do not grow with them.
pub(super) fn load_epochs(layers: &Arc<Layers>) -> Result<Vec<Epoch>, EngineError> {
    let view = View::new(u64::MAX, Arc::clone(layers));
    let mut epochs: Vec<Epoch> = Vec::new();
    view.scan(
        history_table(),
        Bound::Unbounded,
        Bound::Unbounded,
        &mut |key, value| {
            let epoch = decode_epoch(key, value).map_err(StorageError::from)?;
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
        .map(|epoch| EpochId {
            term: epoch.term,
            writer_id: epoch.writer_id.clone(),
        })
        .unwrap_or_default()
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

fn refuse(kind: ErrorKind, message: String) -> EngineError {
    EngineError::Replication(kind, message)
}

fn conflict(held: &EpochId, offered: &EpochId, newest: &EpochId, lsn: u64) -> EngineError {
    if offered.term < newest.term {
        refuse(
            ErrorKind::Fenced,
            format!(
                "a change at LSN {lsn} of term {} comes from a writer that term {} replaced",
                offered.term, newest.term
            ),
        )
    } else {
        refuse(
            ErrorKind::Diverged,
            format!(
                "LSN {lsn} is held in term {} and offered in term {} by another writer",
                held.term, offered.term
            ),
        )
    }
}

impl<F: FileSystem + 'static> Engine<F> {
    /// Every term of the history, oldest first.
    pub(crate) fn epochs(&self) -> Vec<Epoch> {
        lock(&self.shared.epochs).clone()
    }

    /// The epoch and LSN of the last change readers see.
    pub(crate) fn position(&self) -> (EpochId, u64) {
        let lsn = lock(&self.shared.published).view.snapshot;
        (epoch_at(&lock(&self.shared.epochs), lsn), lsn)
    }

    /// Waits until no write transaction is open, so every commit begun before has ended.
    pub(crate) fn quiesce(&self) {
        drop(lock(&self.shared.writer));
    }

    /// The records after `after` that readers see, from the log segments still on disk, sealed
    /// for a batch of changes. They stop at the first record past `max_bytes`, but hold at least
    /// one record when there is any.
    pub(crate) fn read_changes(
        &self,
        after: u64,
        max_bytes: usize,
    ) -> Result<Vec<Vec<u8>>, EngineError> {
        let shared = &*self.shared;
        let upto = lock(&shared.published).view.snapshot;
        if after >= upto {
            return Ok(Vec::new());
        }
        let database_id = lock(&shared.durable).header.database_id;
        let fs = &*shared.fs;
        let sequences = segment_files(fs, &shared.name)?;
        let mut start = None;
        for (index, sequence) in sequences.iter().enumerate().rev() {
            let newest = index + 1 == sequences.len();
            let segment = match open_segment(fs, &shared.name, *sequence, database_id) {
                Ok(segment) => segment,
                Err(_) if newest => continue,
                Err(error) if is_gone(&error) => break,
                Err(error) => return Err(error),
            };
            if segment.1.first_lsn <= after + 1 {
                start = Some((index, segment));
                break;
            }
        }
        let Some((first, segment)) = start else {
            return Err(too_far_behind(after));
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
                    Err(error) => return Err(error),
                },
            };
            let done = read_segment(&file, &header, after, upto, &mut |record| {
                bytes += record.len();
                records.push(reseal(record)?);
                Ok(bytes >= max_bytes)
            })?;
            if done {
                break;
            }
        }
        if records.is_empty() {
            return Err(too_far_behind(after));
        }
        Ok(records)
    }

    /// Applies `changes` in order: logs each record that follows the last one held, under the
    /// writer lock, and skips each one already held in the same epoch. Stops at the first
    /// record it refuses, with the earlier ones applied.
    pub(crate) fn apply_changes(&self, changes: &Decoded) -> Result<(), EngineError> {
        let shared = &*self.shared;
        let mut writer = lock(&shared.writer);
        if shared.is_poisoned() {
            return Err(shared.refusal());
        }
        if changes.after_lsn > writer.last_lsn {
            return Err(refuse(
                ErrorKind::Gap,
                format!(
                    "the changes follow LSN {}, but the database holds changes up to LSN {}",
                    changes.after_lsn, writer.last_lsn
                ),
            ));
        }
        let mut offered = changes.after.clone();
        {
            let epochs = lock(&shared.epochs);
            let held = epoch_at(&epochs, changes.after_lsn);
            if changes.after_lsn > 0 && held != offered {
                return Err(conflict(&held, &offered, &top(&epochs), changes.after_lsn));
            }
        }
        for change in &changes.records {
            let record = &change.record;
            if let Body::Config { writer_id } = &record.body {
                offered = EpochId {
                    term: record.term,
                    writer_id: writer_id.clone(),
                };
            }
            let (held, newest) = {
                let epochs = lock(&shared.epochs);
                (epoch_at(&epochs, record.lsn), top(&epochs))
            };
            if record.lsn <= writer.last_lsn {
                if held != offered {
                    return Err(conflict(&held, &offered, &newest, record.lsn));
                }
                continue;
            }
            let begins = matches!(record.body, Body::Config { .. });
            if offered.term < newest.term || (offered.term == newest.term && begins) {
                return Err(conflict(&newest, &offered, &newest, record.lsn));
            }
            if offered.term == newest.term && offered != newest {
                return Err(conflict(&newest, &offered, &newest, record.lsn));
            }
            self.apply_record(&mut writer, change)?;
        }
        Ok(())
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
        if let Body::Config { writer_id } = &record.body {
            self.begin_term(writer, record.term, writer_id, record.lsn);
        }
        self.publish(writer, record.lsn, staged);
        Ok(())
    }

    fn begin_term(&self, writer: &mut Writer<F>, term: u64, writer_id: &[u8], lsn: u64) {
        writer.term = term;
        lock(&self.shared.epochs).push(Epoch {
            term,
            writer_id: writer_id.to_vec(),
            start_lsn: lsn,
        });
    }

    /// Begins `term` under the writer `writer_id` with a configuration record, or does nothing
    /// when that is already the newest term. Fails as fenced for a lower term and as diverged
    /// for the newest term under another writer.
    pub(crate) fn promote(&self, term: u64, writer_id: &[u8]) -> Result<(), EngineError> {
        let shared = &*self.shared;
        let mut writer = lock(&shared.writer);
        if shared.is_poisoned() {
            return Err(shared.refusal());
        }
        let newest = top(&lock(&shared.epochs));
        let offered = EpochId {
            term,
            writer_id: writer_id.to_vec(),
        };
        if offered == newest {
            return Ok(());
        }
        if term <= newest.term {
            return Err(conflict(&newest, &offered, &newest, writer.last_lsn + 1));
        }
        self.drop_segments_aside(&mut writer);
        let lsn = writer.last_lsn + 1;
        let hlc = hybrid_clock_after(writer.last_hlc);
        let record = config_record(lsn, term, hlc, writer_id)?;
        let mut staged = Staged::new(lock(&shared.published).view.clone());
        stage_record(
            &mut staged,
            lsn,
            term,
            Body::Config {
                writer_id: writer_id.to_vec(),
            },
        )?;
        self.append(&mut writer, record, lsn, hlc)?;
        self.begin_term(&mut writer, term, writer_id, lsn);
        self.publish(&mut writer, lsn, staged);
        Ok(())
    }
}

fn too_far_behind(after: u64) -> EngineError {
    refuse(
        ErrorKind::TooFarBehind,
        format!("the log no longer holds the changes after LSN {after}"),
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
    take: &mut dyn FnMut(Vec<u8>) -> Result<bool, EngineError>,
) -> Result<bool, EngineError> {
    let seed = header.record_seed();
    let blocks = file.block_count()?;
    let mut lsn = header.first_lsn;
    let mut at = 1;
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
        let span = (padded(length) / WAL_BLOCK) as u64;
        if span == 0 || at + span > blocks {
            break;
        }
        buffer.resize(padded(length), 0);
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
            if take(std::mem::take(&mut buffer))? {
                return Ok(true);
            }
        }
        lsn += 1;
        at += span;
    }
    Ok(lsn > upto)
}
