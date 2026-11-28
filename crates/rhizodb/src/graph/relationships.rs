use crate::value::DecodeError;
use crate::value::Reader;
use crate::value::put_varint;

use super::GraphError;
use super::ids::RelationshipId;

pub(crate) const CHUNK_SHIFT: u32 = 5;
/// How many consecutive relationship IDs one chunk covers.
pub(crate) const CHUNK_SLOTS: usize = 1 << CHUNK_SHIFT;
const LAYOUT_VERSION: u8 = 2;
const UNSEALED_LAYOUT: u8 = 1;
const CHECKSUM_AT: usize = 1;
const PRESENT_AT: usize = 5;

const UNKNOWN_LAYOUT: GraphError =
    GraphError::Corrupt("a relationship chunk has an unknown layout");
const CHECKSUM_MISMATCH: GraphError =
    GraphError::Corrupt("a relationship chunk does not match its checksum");
const EMPTY_CHUNK: GraphError = GraphError::Corrupt("a stored relationship chunk is empty");

/// The chunk holding `id` and the relationship's slot in it.
pub(crate) fn locate(id: RelationshipId) -> (u64, usize) {
    (
        id.0 >> CHUNK_SHIFT,
        (id.0 & (CHUNK_SLOTS as u64 - 1)) as usize,
    )
}

pub(crate) fn relationship_at(chunk: u64, slot: usize) -> RelationshipId {
    RelationshipId((chunk << CHUNK_SHIFT) | slot as u64)
}

pub(crate) fn chunk_key(chunk: u64) -> [u8; 8] {
    chunk.to_be_bytes()
}

pub(crate) fn parse_key(key: &[u8]) -> Result<u64, DecodeError> {
    let mut reader = Reader::new(key);
    let chunk = reader.u64()?;
    reader.finish()?;
    Ok(chunk)
}

fn checksum(key: &[u8], body: &[u8]) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(key);
    hasher.update(body);
    hasher.finalize()
}

/// The chunk stored under `key` in the layout of graph format 14, rewritten with its checksum,
/// or `None` when it already carries one that matches. Fails when it is in neither layout, or
/// its records do not frame, so the upgrade seals no damaged chunk.
pub(crate) fn seal(key: &[u8], bytes: &[u8]) -> Result<Option<Vec<u8>>, GraphError> {
    match bytes.first() {
        Some(&LAYOUT_VERSION) => ChunkRef::parse(key, bytes).map(|_| None),
        Some(&UNSEALED_LAYOUT) => {
            for entry in ChunkRef::body(&bytes[1..])?.entries() {
                entry?;
            }
            let mut sealed = Vec::with_capacity(bytes.len() + 4);
            sealed.push(LAYOUT_VERSION);
            sealed.extend_from_slice(&checksum(key, &bytes[1..]).to_le_bytes());
            sealed.extend_from_slice(&bytes[1..]);
            Ok(Some(sealed))
        }
        _ => Err(UNKNOWN_LAYOUT),
    }
}

/// A chunk in the layout of graph format 14, which carried no checksum.
#[cfg(test)]
pub(crate) fn unsealed(bytes: &[u8]) -> Vec<u8> {
    let mut unsealed = vec![UNSEALED_LAYOUT];
    unsealed.extend_from_slice(&bytes[PRESENT_AT..]);
    unsealed
}

/// A stored chunk read in place: a layout byte, the little-endian CRC-32 of its key and of the
/// bytes after it, a little-endian bitmap of the slots that hold a relationship, and the record
/// of each of those slots in slot order, each after its length as a varint.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ChunkRef<'a> {
    present: u32,
    records: &'a [u8],
}

impl<'a> ChunkRef<'a> {
    /// The chunk stored under `key`, checked against its checksum.
    pub(crate) fn parse(key: &[u8], bytes: &'a [u8]) -> Result<Self, GraphError> {
        let mut reader = Reader::new(bytes);
        if reader.u8()? != LAYOUT_VERSION {
            return Err(UNKNOWN_LAYOUT);
        }
        let mut stored = [0u8; 4];
        stored.copy_from_slice(reader.take(4)?);
        let body = &bytes[PRESENT_AT..];
        if checksum(key, body) != u32::from_le_bytes(stored) {
            return Err(CHECKSUM_MISMATCH);
        }
        Self::body(body)
    }

    fn body(bytes: &'a [u8]) -> Result<Self, GraphError> {
        let mut reader = Reader::new(bytes);
        let mut present = [0u8; 4];
        present.copy_from_slice(reader.take(4)?);
        let present = u32::from_le_bytes(present);
        if present == 0 {
            return Err(EMPTY_CHUNK);
        }
        let records = reader.take(reader.remaining())?;
        Ok(Self { present, records })
    }

    /// The record in `slot`, reading past the lengths of the records before it.
    pub(crate) fn get(&self, slot: usize) -> Result<Option<&'a [u8]>, GraphError> {
        if self.present & (1 << slot) == 0 {
            return Ok(None);
        }
        let mut reader = Reader::new(self.records);
        for _ in 0..(self.present & ((1 << slot) - 1)).count_ones() {
            let length = reader.varint()?;
            reader.take(length)?;
        }
        let length = reader.varint()?;
        Ok(Some(reader.take(length)?))
    }

    /// Every slot that holds a relationship, ascending, with its record. The last item fails if
    /// the chunk holds bytes past its last record.
    pub(crate) fn entries(&self) -> impl Iterator<Item = Result<(usize, &'a [u8]), GraphError>> {
        let mut reader = Reader::new(self.records);
        let mut present = self.present;
        let mut failed = false;
        std::iter::from_fn(move || {
            if failed {
                return None;
            }
            if present == 0 {
                failed = true;
                return reader.finish().err().map(|error| Err(error.into()));
            }
            let slot = present.trailing_zeros() as usize;
            present &= present - 1;
            let record = reader
                .varint()
                .and_then(|length| reader.take(length))
                .map(|record| (slot, record))
                .map_err(GraphError::from);
            failed = record.is_err();
            Some(record)
        })
    }
}

/// A chunk held decoded while a write transaction changes it.
#[derive(Clone, Debug, Default)]
pub(crate) struct ChunkEdit {
    records: Vec<(u8, Box<[u8]>)>,
    bytes: usize,
}

impl ChunkEdit {
    /// The chunk stored under `key`, checked against its checksum.
    pub(crate) fn load(key: &[u8], bytes: &[u8]) -> Result<Self, GraphError> {
        let mut edit = Self::default();
        for entry in ChunkRef::parse(key, bytes)?.entries() {
            let (slot, record) = entry?;
            edit.bytes += record.len();
            edit.records.push((slot as u8, record.into()));
        }
        Ok(edit)
    }

    /// The bytes of the records it holds.
    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }

    pub(crate) fn get(&self, slot: usize) -> Option<&[u8]> {
        self.position(slot)
            .ok()
            .map(|at| self.records[at].1.as_ref())
    }

    /// Puts `record` into `slot`, or empties the slot when it is `None`.
    pub(crate) fn set(&mut self, slot: usize, record: Option<&[u8]>) {
        match (self.position(slot), record) {
            (Ok(at), Some(record)) => {
                self.bytes = self.bytes - self.records[at].1.len() + record.len();
                self.records[at].1 = Box::from(record);
            }
            (Ok(at), None) => {
                self.bytes -= self.records.remove(at).1.len();
            }
            (Err(at), Some(record)) => {
                self.bytes += record.len();
                self.records.insert(at, (slot as u8, Box::from(record)));
            }
            (Err(_), None) => {}
        }
    }

    pub(crate) fn entries(&self) -> impl Iterator<Item = (usize, &[u8])> {
        self.records
            .iter()
            .map(|(slot, record)| (usize::from(*slot), record.as_ref()))
    }

    /// The form of the chunk stored under `key`, or `None` when it holds no relationship.
    pub(crate) fn encode(&self, key: &[u8]) -> Option<Vec<u8>> {
        if self.records.is_empty() {
            return None;
        }
        let mut present = 0u32;
        let mut out = Vec::with_capacity(PRESENT_AT + 4 + self.bytes + 2 * self.records.len());
        out.push(LAYOUT_VERSION);
        out.extend_from_slice(&[0; 8]);
        for (slot, record) in &self.records {
            present |= 1 << slot;
            put_varint(&mut out, record.len());
            out.extend_from_slice(record);
        }
        out[PRESENT_AT..PRESENT_AT + 4].copy_from_slice(&present.to_le_bytes());
        let sum = checksum(key, &out[PRESENT_AT..]);
        out[CHECKSUM_AT..PRESENT_AT].copy_from_slice(&sum.to_le_bytes());
        Some(out)
    }

    fn position(&self, slot: usize) -> Result<usize, usize> {
        self.records
            .binary_search_by_key(&slot, |(held, _)| usize::from(*held))
    }
}

/// Writes the checksum of a chunk stored under `key` again over whatever its bytes now hold, so
/// a fuzzer's damage reaches the parser behind the check.
#[cfg(any(test, fuzzing))]
pub(crate) fn reseal_for_fuzzing(key: &[u8], bytes: &mut [u8]) {
    if bytes.len() >= PRESENT_AT {
        let sum = checksum(key, &bytes[PRESENT_AT..]);
        bytes[CHECKSUM_AT..PRESENT_AT].copy_from_slice(&sum.to_le_bytes());
    }
}

#[cfg(test)]
#[path = "relationships.test.rs"]
mod tests;
