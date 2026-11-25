use super::codec::Reader;
use super::codec::checksum;
use super::codec::put_bytes;
use super::codec::put_u16;
use super::codec::put_u32;
use super::codec::put_u64;
use super::codec::put_u128;
use super::error::EngineError;
use super::header::FORMAT_MAJOR;
use super::header::FORMAT_MINOR;
use super::layout::TableId;
use super::layout::WAL_BLOCK;

pub(crate) const SEGMENT_MAGIC: [u8; 8] = *b"RZWALSEG";

/// The bytes of a record before its body.
pub(crate) const RECORD_HEADER: usize = 54;

/// The longest record the log accepts, so a damaged length never makes a reader allocate more.
pub(crate) const RECORD_LIMIT: usize = 1 << 30;

const SEGMENT_CHECKSUM_AT: usize = WAL_BLOCK - 8;

const BATCH_FORMAT: u8 = 1;
const CHECKPOINT_FORMAT: u8 = 1;

/// The first block of every segment, which names the database and log it belongs to, so a segment
/// can be shipped to another node and replayed there on its own.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SegmentHeader {
    pub(crate) minor: u16,
    pub(crate) database_id: u128,
    pub(crate) shard_id: u64,
    pub(crate) sequence: u64,
    pub(crate) first_lsn: u64,
    pub(crate) first_term: u64,
    pub(crate) created_hlc: u64,
    pub(crate) salt: u64,
}

impl SegmentHeader {
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(WAL_BLOCK);
        out.extend_from_slice(&SEGMENT_MAGIC);
        put_u16(&mut out, FORMAT_MAJOR);
        put_u16(&mut out, self.minor);
        put_u32(&mut out, 0);
        put_u128(&mut out, self.database_id);
        put_u64(&mut out, self.shard_id);
        put_u64(&mut out, self.sequence);
        put_u64(&mut out, self.first_lsn);
        put_u64(&mut out, self.first_term);
        put_u64(&mut out, self.created_hlc);
        put_u64(&mut out, self.salt);
        out.resize(SEGMENT_CHECKSUM_AT, 0);
        let sum = checksum(0, &[&out]);
        put_u64(&mut out, sum);
        out
    }

    pub(crate) fn decode(block: &[u8]) -> Result<Self, EngineError> {
        if block.len() != WAL_BLOCK {
            return Err(EngineError::corrupt("a segment header is not one block"));
        }
        let (body, stored) = block.split_at(SEGMENT_CHECKSUM_AT);
        let mut reader = Reader::new(body, "a segment header");
        if reader.take(SEGMENT_MAGIC.len())? != SEGMENT_MAGIC {
            return Err(EngineError::corrupt(
                "a log segment does not start with the segment magic",
            ));
        }
        if Reader::new(stored, "a segment checksum").u64()? != checksum(0, &[body]) {
            return Err(EngineError::corrupt("a segment header fails its checksum"));
        }
        let major = reader.u16()?;
        let minor = reader.u16()?;
        if major != FORMAT_MAJOR || minor > FORMAT_MINOR {
            return Err(EngineError::Unsupported(format!(
                "log segment format {major}.{minor}"
            )));
        }
        if reader.u32()? != 0 {
            return Err(reader.bad("reserved field"));
        }
        let header = Self {
            minor,
            database_id: reader.u128()?,
            shard_id: reader.u64()?,
            sequence: reader.u64()?,
            first_lsn: reader.u64()?,
            first_term: reader.u64()?,
            created_hlc: reader.u64()?,
            salt: reader.u64()?,
        };
        if reader.rest().iter().any(|byte| *byte != 0) {
            return Err(reader.bad("reserved area"));
        }
        if header.first_lsn == 0 {
            return Err(reader.bad("first LSN"));
        }
        Ok(header)
    }

    /// The seed of the checksum of every record in this segment, so a record left from another
    /// log or segment never passes as one of this segment.
    pub(crate) fn record_seed(&self) -> u64 {
        self.salt ^ self.sequence.rotate_left(32)
    }
}

/// One operation of a batch, applied in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Op {
    Put {
        table: TableId,
        key: Vec<u8>,
        value: Vec<u8>,
    },
    Delete {
        table: TableId,
        key: Vec<u8>,
    },
    DeleteRange {
        table: TableId,
        start: Vec<u8>,
        end: Vec<u8>,
    },
}

const PUT: u8 = 1;
const DELETE: u8 = 2;
const DELETE_RANGE: u8 = 3;
const PATCH: u8 = 4;

/// What a record holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Body {
    /// The writes of one committed transaction.
    Batch(Vec<Op>),
    /// A checkpoint at `lsn` reached the database file in header generation `generation`.
    Checkpoint { lsn: u64, generation: u64 },
}

/// The kinds of record. Kinds this build reserves but cannot apply refuse the segment, since
/// skipping one could drop a write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Batch,
    Ingest,
    Checkpoint,
    ClosedTimestamp,
    Config,
    NoOp,
    Intent,
    Prepare,
    Commit,
    Abort,
}

impl Kind {
    pub(crate) const ALL: [Kind; 10] = [
        Kind::Batch,
        Kind::Ingest,
        Kind::Checkpoint,
        Kind::ClosedTimestamp,
        Kind::Config,
        Kind::NoOp,
        Kind::Intent,
        Kind::Prepare,
        Kind::Commit,
        Kind::Abort,
    ];

    pub(crate) fn code(self) -> u8 {
        match self {
            Kind::Batch => 1,
            Kind::Ingest => 2,
            Kind::Checkpoint => 3,
            Kind::ClosedTimestamp => 16,
            Kind::Config => 17,
            Kind::NoOp => 18,
            Kind::Intent => 19,
            Kind::Prepare => 20,
            Kind::Commit => 21,
            Kind::Abort => 22,
        }
    }

    fn from_code(code: u8) -> Option<Kind> {
        Kind::ALL.into_iter().find(|kind| kind.code() == code)
    }
}

/// One log record. A local engine writes term 0.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Record {
    pub(crate) lsn: u64,
    pub(crate) term: u64,
    pub(crate) hlc: u64,
    pub(crate) txn: u128,
    pub(crate) body: Body,
}

/// Appends one operation to a batch body under construction.
pub(crate) fn encode_op(out: &mut Vec<u8>, op: OpRef<'_>) -> Result<(), EngineError> {
    match op {
        OpRef::Put { table, key, value } => {
            out.push(PUT);
            put_u16(out, table.0);
            put_bytes(out, key)?;
            put_bytes(out, value)?;
        }
        OpRef::Delete { table, key } => {
            out.push(DELETE);
            put_u16(out, table.0);
            put_bytes(out, key)?;
        }
        OpRef::DeleteRange { table, start, end } => {
            out.push(DELETE_RANGE);
            put_u16(out, table.0);
            put_bytes(out, start)?;
            put_bytes(out, end)?;
        }
    }
    Ok(())
}

/// An operation borrowed from a transaction, encoded without copying its bytes first.
#[derive(Clone, Copy, Debug)]
pub(crate) enum OpRef<'a> {
    Put {
        table: TableId,
        key: &'a [u8],
        value: &'a [u8],
    },
    Delete {
        table: TableId,
        key: &'a [u8],
    },
    DeleteRange {
        table: TableId,
        start: &'a [u8],
        end: &'a [u8],
    },
}

impl Op {
    #[cfg(any(test, feature = "fuzzing"))]
    pub(crate) fn borrowed(&self) -> OpRef<'_> {
        match self {
            Op::Put { table, key, value } => OpRef::Put {
                table: *table,
                key,
                value,
            },
            Op::Delete { table, key } => OpRef::Delete { table: *table, key },
            Op::DeleteRange { table, start, end } => OpRef::DeleteRange {
                table: *table,
                start,
                end,
            },
        }
    }
}

/// The record's bytes before its body, with the length and checksum left as zeros.
pub(crate) fn record_prefix(
    lsn: u64,
    term: u64,
    hlc: u64,
    txn: u128,
    kind: Kind,
    format: u8,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(RECORD_HEADER);
    put_u32(&mut out, 0);
    put_u64(&mut out, 0);
    put_u64(&mut out, lsn);
    put_u64(&mut out, term);
    put_u64(&mut out, hlc);
    put_u128(&mut out, txn);
    out.push(kind.code());
    out.push(format);
    out
}

/// Fills in the length and checksum of a record whose prefix and body are in `record`.
pub(crate) fn seal_record(seed: u64, record: &mut [u8]) -> Result<(), EngineError> {
    if record.len() < RECORD_HEADER || record.len() > RECORD_LIMIT {
        return Err(EngineError::TooLarge);
    }
    let length = record.len() as u32;
    record[..4].copy_from_slice(&length.to_le_bytes());
    let sum = checksum(seed, &[&record[..4], &record[12..]]);
    record[4..12].copy_from_slice(&sum.to_le_bytes());
    Ok(())
}

/// A batch record of `ops`, to be sealed by the segment it lands in.
pub(crate) fn batch_record<'a>(
    lsn: u64,
    hlc: u64,
    txn: u128,
    ops: impl ExactSizeIterator<Item = OpRef<'a>>,
) -> Result<Vec<u8>, EngineError> {
    let mut record = record_prefix(lsn, 0, hlc, txn, Kind::Batch, BATCH_FORMAT);
    put_u32(
        &mut record,
        u32::try_from(ops.len()).map_err(|_| EngineError::TooLarge)?,
    );
    for op in ops {
        encode_op(&mut record, op)?;
        if record.len() > RECORD_LIMIT {
            return Err(EngineError::TooLarge);
        }
    }
    Ok(record)
}

#[cfg(any(test, feature = "fuzzing"))]
pub(crate) fn checkpoint_record(
    lsn: u64,
    hlc: u64,
    checkpoint_lsn: u64,
    generation: u64,
) -> Vec<u8> {
    let mut record = record_prefix(
        lsn,
        0,
        hlc,
        u128::from(lsn),
        Kind::Checkpoint,
        CHECKPOINT_FORMAT,
    );
    put_u64(&mut record, checkpoint_lsn);
    put_u64(&mut record, generation);
    record
}

/// Why the bytes at a place in a segment are not a record.
#[derive(Debug)]
pub(crate) enum NotRecord {
    /// Zeros where a record would start: nothing was written here.
    End,
    /// A record that is cut short or fails its checksum, as a torn write leaves.
    Invalid(String),
    /// A record that passes its checksum but that this build cannot apply.
    Refused(EngineError),
}

/// The total length the record at the start of `bytes` claims, or why there is none.
pub(crate) fn record_length(bytes: &[u8]) -> Result<usize, NotRecord> {
    let Some(head) = bytes.get(..4) else {
        return Err(NotRecord::End);
    };
    let mut array = [0; 4];
    array.copy_from_slice(head);
    let length = u32::from_le_bytes(array) as usize;
    if length == 0 {
        return Err(NotRecord::End);
    }
    if !(RECORD_HEADER..=RECORD_LIMIT).contains(&length) {
        return Err(NotRecord::Invalid(format!(
            "a record claims {length} bytes"
        )));
    }
    if length > bytes.len() {
        return Err(NotRecord::Invalid("a record is cut short".to_owned()));
    }
    Ok(length)
}

/// Decodes the record at the start of `bytes`, checked with `seed`, and returns it with its length.
pub(crate) fn decode_record(seed: u64, bytes: &[u8]) -> Result<(Record, usize), NotRecord> {
    let length = record_length(bytes)?;
    let record = &bytes[..length];
    let mut reader = Reader::new(&record[4..], "a log record");
    let stored = reader
        .u64()
        .map_err(|error| NotRecord::Invalid(error.to_string()))?;
    if stored != checksum(seed, &[&record[..4], &record[12..]]) {
        return Err(NotRecord::Invalid("a record fails its checksum".to_owned()));
    }
    decode_checked(reader)
        .map(|decoded| (decoded, length))
        .map_err(NotRecord::Refused)
}

fn decode_checked(mut reader: Reader<'_>) -> Result<Record, EngineError> {
    let lsn = reader.u64()?;
    let term = reader.u64()?;
    let hlc = reader.u64()?;
    let txn = reader.u128()?;
    let code = reader.u8()?;
    let format = reader.u8()?;
    let kind = Kind::from_code(code)
        .ok_or_else(|| EngineError::Unsupported(format!("log record kind {code}")))?;
    let body = match (kind, format) {
        (Kind::Batch, BATCH_FORMAT) => Body::Batch(decode_batch(&mut reader)?),
        (Kind::Checkpoint, CHECKPOINT_FORMAT) => Body::Checkpoint {
            lsn: reader.u64()?,
            generation: reader.u64()?,
        },
        (kind, format) => {
            return Err(EngineError::Unsupported(format!(
                "log record kind {kind:?} in body format {format}"
            )));
        }
    };
    reader.finish()?;
    if lsn == 0 {
        return Err(reader.bad("LSN"));
    }
    Ok(Record {
        lsn,
        term,
        hlc,
        txn,
        body,
    })
}

fn decode_batch(reader: &mut Reader<'_>) -> Result<Vec<Op>, EngineError> {
    let count = reader.u32()? as usize;
    let mut ops = Vec::with_capacity(count.min(reader.remaining() / 7));
    for _ in 0..count {
        let tag = reader.u8()?;
        let table = TableId(reader.u16()?);
        ops.push(match tag {
            PUT => Op::Put {
                table,
                key: reader.bytes()?.to_vec(),
                value: reader.bytes()?.to_vec(),
            },
            DELETE => Op::Delete {
                table,
                key: reader.bytes()?.to_vec(),
            },
            DELETE_RANGE => Op::DeleteRange {
                table,
                start: reader.bytes()?.to_vec(),
                end: reader.bytes()?.to_vec(),
            },
            PATCH => {
                return Err(EngineError::Unsupported(
                    "the patch operation of a batch".to_owned(),
                ));
            }
            tag => {
                return Err(EngineError::Unsupported(format!("batch operation {tag}")));
            }
        });
    }
    Ok(ops)
}

/// The bytes a record of `length` takes in a segment, rounded up to whole blocks so the next
/// record starts in a block of its own.
pub(crate) fn padded(length: usize) -> usize {
    length.div_ceil(WAL_BLOCK) * WAL_BLOCK
}

#[cfg(test)]
#[path = "wal_format.test.rs"]
mod tests;
