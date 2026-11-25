use crate::value::DecodeError;
use crate::value::Reader;
use crate::value::put_varint;

use super::GraphError;
use super::ids::RelationshipId;

const CHUNK_SHIFT: u32 = 5;
/// How many consecutive relationship IDs one chunk covers.
pub(crate) const CHUNK_SLOTS: usize = 1 << CHUNK_SHIFT;
const LAYOUT_VERSION: u8 = 1;

const UNKNOWN_LAYOUT: GraphError =
    GraphError::Corrupt("a relationship chunk has an unknown layout");
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

/// A stored chunk read in place: a layout byte, a little-endian bitmap of the slots that hold a
/// relationship, and the record of each of those slots in slot order, each after its length as
/// a varint.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ChunkRef<'a> {
    present: u32,
    records: &'a [u8],
}

impl<'a> ChunkRef<'a> {
    pub(crate) fn parse(bytes: &'a [u8]) -> Result<Self, GraphError> {
        let mut reader = Reader::new(bytes);
        if reader.u8()? != LAYOUT_VERSION {
            return Err(UNKNOWN_LAYOUT);
        }
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
    pub(crate) fn load(bytes: &[u8]) -> Result<Self, GraphError> {
        let mut edit = Self::default();
        for entry in ChunkRef::parse(bytes)?.entries() {
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

    /// The stored form of the chunk, or `None` when it holds no relationship.
    pub(crate) fn encode(&self) -> Option<Vec<u8>> {
        if self.records.is_empty() {
            return None;
        }
        let mut present = 0u32;
        let mut out = Vec::with_capacity(5 + self.bytes + 2 * self.records.len());
        out.push(LAYOUT_VERSION);
        out.extend_from_slice(&[0; 4]);
        for (slot, record) in &self.records {
            present |= 1 << slot;
            put_varint(&mut out, record.len());
            out.extend_from_slice(record);
        }
        out[1..5].copy_from_slice(&present.to_le_bytes());
        Some(out)
    }

    fn position(&self, slot: usize) -> Result<usize, usize> {
        self.records
            .binary_search_by_key(&slot, |(held, _)| usize::from(*held))
    }
}

#[cfg(test)]
#[path = "relationships.test.rs"]
mod tests;
