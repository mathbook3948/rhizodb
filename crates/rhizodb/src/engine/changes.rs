use super::codec::Reader;
use super::codec::checksum;
use super::codec::put_u32;
use super::codec::put_u64;
use super::codec::put_u128;
use super::error::EngineError;
use super::layout::TableId;
use super::wal_format::Body;
use super::wal_format::NotRecord;
use super::wal_format::Op;
use super::wal_format::RECORD_HEADER;
use super::wal_format::Record;
use super::wal_format::Step;
use super::wal_format::chain_after;
use super::wal_format::decode_record;
use super::wal_format::seal_record;
use crate::replication::WRITER_ID_LIMIT;

const MAGIC: [u8; 8] = *b"RZCHANGE";
const FORMAT: u8 = 1;

/// The seed of every record's checksum inside a batch of changes, so a record copied out of a
/// log segment never passes as one of a change batch unsealed.
pub(crate) const CHANGES_SEED: u64 = 0x5248_495A_4F43_4847;

/// The epoch of a position: its term and writer id.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct EpochId {
    pub(crate) term: u64,
    pub(crate) writer_id: Vec<u8>,
}

/// One whole transaction or configuration record, decoded and as the bytes it travels in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ChangeRecord {
    pub(crate) record: Record,
    pub(crate) bytes: Vec<u8>,
}

/// Changes that follow `after_lsn`, which the source holds in epoch `after` with chain hash
/// `after_chain`, and end in epoch `last` with chain hash `last_chain`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Decoded {
    pub(crate) database: u128,
    pub(crate) after: EpochId,
    pub(crate) after_lsn: u64,
    pub(crate) after_chain: u64,
    pub(crate) records: Vec<ChangeRecord>,
    pub(crate) last: EpochId,
    pub(crate) last_chain: u64,
}

impl Decoded {
    pub(crate) fn last_lsn(&self) -> u64 {
        self.records
            .last()
            .map_or(self.after_lsn, |change| change.record.lsn)
    }

    /// Whether any record writes `table`.
    pub(crate) fn touches(&self, table: TableId) -> bool {
        self.records.iter().any(|change| match &change.record.body {
            Body::Batch(ops) => ops.iter().any(|op| match op {
                Op::Put { table: id, .. }
                | Op::Delete { table: id, .. }
                | Op::DeleteRange { table: id, .. } => *id == table,
            }),
            Body::Checkpoint { .. } | Body::Config { .. } => false,
        })
    }
}

/// Reseals a record read from a log segment for a batch of changes.
pub(crate) fn reseal(mut bytes: Vec<u8>) -> Result<Vec<u8>, EngineError> {
    seal_record(CHANGES_SEED, &mut bytes)?;
    Ok(bytes)
}

/// The bytes of the changes `records`, each sealed with [`CHANGES_SEED`], that follow
/// `after_lsn` in epoch `after`.
pub(crate) fn encode(
    database: u128,
    after: &EpochId,
    after_lsn: u64,
    after_chain: u64,
    records: &[Vec<u8>],
) -> Result<Vec<u8>, EngineError> {
    let length = records.iter().map(Vec::len).sum::<usize>();
    let mut out = Vec::with_capacity(48 + after.writer_id.len() + length);
    out.extend_from_slice(&MAGIC);
    out.push(FORMAT);
    put_u128(&mut out, database);
    put_u64(&mut out, after.term);
    put_u64(&mut out, after_lsn);
    put_u64(&mut out, after_chain);
    out.push(u8::try_from(after.writer_id.len()).map_err(|_| EngineError::TooLarge)?);
    out.extend_from_slice(&after.writer_id);
    put_u32(
        &mut out,
        u32::try_from(records.len()).map_err(|_| EngineError::TooLarge)?,
    );
    for record in records {
        out.extend_from_slice(record);
    }
    let sum = checksum(CHANGES_SEED, &[&out]);
    put_u64(&mut out, sum);
    Ok(out)
}

fn bad(what: impl Into<String>) -> EngineError {
    EngineError::corrupt(format!("the changes are malformed: {}", what.into()))
}

fn valid_epoch(epoch: &EpochId) -> bool {
    match epoch.term {
        0 => epoch.writer_id.is_empty(),
        _ => (1..=WRITER_ID_LIMIT).contains(&epoch.writer_id.len()),
    }
}

/// Reads changes from untrusted bytes, checking their checksums, that their LSNs follow one
/// another, and that each term begins with a configuration record of a higher term.
pub(crate) fn decode(bytes: &[u8]) -> Result<Decoded, EngineError> {
    if bytes.len() < MAGIC.len() + 8 {
        return Err(bad("too short"));
    }
    let (body, stored) = bytes.split_at(bytes.len() - 8);
    if Reader::new(stored, "a change checksum").u64()? != checksum(CHANGES_SEED, &[body]) {
        return Err(bad("they fail their checksum"));
    }
    let mut reader = Reader::new(body, "changes");
    if reader.take(MAGIC.len())? != MAGIC {
        return Err(bad("no magic"));
    }
    if reader.u8()? != FORMAT {
        return Err(bad("an unknown format"));
    }
    let database = reader.u128()?;
    let term = reader.u64()?;
    let after_lsn = reader.u64()?;
    let after_chain = reader.u64()?;
    let length = usize::from(reader.u8()?);
    let after = EpochId {
        term,
        writer_id: reader.take(length)?.to_vec(),
    };
    if !valid_epoch(&after) {
        return Err(bad("the epoch they follow"));
    }
    let count = reader.u32()? as usize;
    if count > reader.remaining() / RECORD_HEADER {
        return Err(bad("more records than bytes"));
    }
    let mut rest = reader.rest();
    let mut records = Vec::with_capacity(count);
    let mut last = after.clone();
    let mut chain = after_chain;
    let mut lsn = after_lsn;
    for _ in 0..count {
        let (record, length) = decode_record(CHANGES_SEED, rest).map_err(|error| match error {
            NotRecord::End => bad("a record is missing"),
            NotRecord::Invalid(reason) | NotRecord::Refused(EngineError::Corrupt(reason)) => {
                bad(reason)
            }
            NotRecord::Refused(error) => bad(error.to_string()),
        })?;
        lsn = lsn.checked_add(1).ok_or_else(|| bad("an LSN overflows"))?;
        if record.lsn != lsn {
            return Err(bad(format!("LSN {} where {lsn} was next", record.lsn)));
        }
        if record.prev != Some(chain) {
            return Err(bad(format!(
                "the record at LSN {lsn} does not follow the record before it"
            )));
        }
        match &record.body {
            Body::Batch(ops) if record.term == last.term => {
                for op in ops {
                    let (Op::Put { table, .. }
                    | Op::Delete { table, .. }
                    | Op::DeleteRange { table, .. }) = op;
                    if table.table().is_err() || table.0 < 0x100 {
                        return Err(bad(format!("a write to table {:#x}", table.0)));
                    }
                }
            }
            Body::Config {
                writer_id,
                step: Step::Begin,
                ..
            } if record.term > last.term => {
                last = EpochId {
                    term: record.term,
                    writer_id: writer_id.clone(),
                };
                if !valid_epoch(&last) {
                    return Err(bad("a configuration names no writer"));
                }
            }
            Body::Config {
                writer_id,
                step: Step::StepDown,
                ..
            } if record.term == last.term && *writer_id == last.writer_id && last.term > 0 => {}
            Body::Batch(_) | Body::Config { .. } => {
                return Err(bad(format!(
                    "a record of term {} where term {} holds",
                    record.term, last.term
                )));
            }
            Body::Checkpoint { .. } => return Err(bad("a checkpoint record")),
        }
        let (bytes, after) = rest.split_at(length);
        chain = chain_after(chain, bytes);
        records.push(ChangeRecord {
            record,
            bytes: bytes.to_vec(),
        });
        rest = after;
    }
    if !rest.is_empty() {
        return Err(bad("bytes after the last record"));
    }
    Ok(Decoded {
        database,
        after,
        after_lsn,
        after_chain,
        records,
        last,
        last_chain: chain,
    })
}

#[cfg(test)]
#[path = "changes.test.rs"]
mod tests;

/// Recomputes every checksum of bytes shaped like changes, so a fuzzer's damage reaches the
/// decoders behind them. Bytes too short to hold the shape come back as they were.
#[cfg(any(test, fuzzing))]
pub(crate) fn reseal_for_fuzzing(mut bytes: Vec<u8>) -> Vec<u8> {
    use super::wal_format::RECORD_LIMIT;
    let Some(&length) = bytes.get(MAGIC.len() + 41) else {
        return bytes;
    };
    let mut at = MAGIC.len() + 42 + usize::from(length) + 4;
    while let Some(head) = bytes.get(at..at + 4) {
        let mut array = [0; 4];
        array.copy_from_slice(head);
        let length = u32::from_le_bytes(array) as usize;
        if !(RECORD_HEADER..=RECORD_LIMIT).contains(&length) || at + length + 8 > bytes.len() {
            break;
        }
        if seal_record(CHANGES_SEED, &mut bytes[at..at + length]).is_err() {
            break;
        }
        at += length;
    }
    if bytes.len() >= 8 {
        let end = bytes.len() - 8;
        let sum = checksum(CHANGES_SEED, &[&bytes[..end]]);
        bytes[end..].copy_from_slice(&sum.to_le_bytes());
    }
    bytes
}
