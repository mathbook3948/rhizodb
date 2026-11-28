use crate::value::DecodeError;
use crate::value::PropertyValue;
use crate::value::Reader;
use crate::value::put_varint;
use crate::value::put_varint_u64;
use crate::value::unzigzag;
use crate::value::zigzag;

use super::GraphError;
use super::ids::RelationshipId;
use super::records::RelationshipRecord;

pub(crate) const CHUNK_SHIFT: u32 = 5;
/// How many consecutive relationship IDs one chunk covers.
pub(crate) const CHUNK_SLOTS: usize = 1 << CHUNK_SHIFT;
const LAYOUT_VERSION: u8 = 3;
const RECORDS_LAYOUT: u8 = 2;
const UNSEALED_LAYOUT: u8 = 1;
const CHECKSUM_AT: usize = 1;
const PRESENT_AT: usize = 5;
const INTEGERS: u8 = 0;
const TAGGED: u8 = 1;
const MAX_WIDTH: u8 = 64;

const UNKNOWN_LAYOUT: GraphError =
    GraphError::Corrupt("a relationship chunk has an unknown layout");
const CHECKSUM_MISMATCH: GraphError =
    GraphError::Corrupt("a relationship chunk does not match its checksum");
const EMPTY_CHUNK: GraphError = GraphError::Corrupt("a stored relationship chunk is empty");
const BAD_COLUMNS: GraphError =
    GraphError::Corrupt("the columns of a relationship chunk do not frame its records");

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

/// The chunk stored under `key` in an older layout, rewritten in the current one, or `None` when
/// it is in the current layout and reads. Fails when it is in no layout, fails its checksum, or
/// holds records that do not frame or decode, so the upgrade puts no damage under a valid
/// checksum.
pub(crate) fn upgrade(key: &[u8], bytes: &[u8]) -> Result<Option<Vec<u8>>, GraphError> {
    let records = match bytes.first() {
        Some(&LAYOUT_VERSION) => return ChunkRef::parse(key, bytes).map(|_| None),
        Some(&RECORDS_LAYOUT) => {
            sealed(key, bytes)?;
            &bytes[PRESENT_AT..]
        }
        Some(&UNSEALED_LAYOUT) => &bytes[1..],
        _ => return Err(UNKNOWN_LAYOUT),
    };
    let mut edit = ChunkEdit::default();
    let (present, records) = present_and_rest(records)?;
    for entry in framed(present, records) {
        let (slot, record) = entry?;
        edit.set(slot, Some(&RelationshipRecord::decode(record)?.encode()));
    }
    edit.encode(key)
}

fn sealed(key: &[u8], bytes: &[u8]) -> Result<(), GraphError> {
    let mut stored = [0u8; 4];
    stored.copy_from_slice(
        bytes
            .get(CHECKSUM_AT..PRESENT_AT)
            .ok_or(DecodeError::UnexpectedEnd)?,
    );
    if checksum(key, &bytes[PRESENT_AT..]) != u32::from_le_bytes(stored) {
        return Err(CHECKSUM_MISMATCH);
    }
    Ok(())
}

fn present_and_rest(bytes: &[u8]) -> Result<(u32, &[u8]), GraphError> {
    let mut reader = Reader::new(bytes);
    let mut present = [0u8; 4];
    present.copy_from_slice(reader.take(4)?);
    let present = u32::from_le_bytes(present);
    if present == 0 {
        return Err(EMPTY_CHUNK);
    }
    Ok((present, reader.take(reader.remaining())?))
}

fn framed(
    present: u32,
    records: &[u8],
) -> impl Iterator<Item = Result<(usize, &[u8]), GraphError>> {
    let mut reader = Reader::new(records);
    let mut present = present;
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

/// A chunk in the layout of graph format 15, which held each record after its length.
#[cfg(test)]
pub(crate) fn in_records_layout(key: &[u8], records: &[(usize, Vec<u8>)]) -> Vec<u8> {
    let mut present = 0u32;
    let mut out = vec![RECORDS_LAYOUT, 0, 0, 0, 0, 0, 0, 0, 0];
    for (slot, record) in records {
        present |= 1 << slot;
        put_varint(&mut out, record.len());
        out.extend_from_slice(record);
    }
    out[PRESENT_AT..PRESENT_AT + 4].copy_from_slice(&present.to_le_bytes());
    let sum = checksum(key, &out[PRESENT_AT..]);
    out[CHECKSUM_AT..PRESENT_AT].copy_from_slice(&sum.to_le_bytes());
    out
}

/// `bytes`, a chunk of layout 2 or 3, under the checksum its key and contents give.
#[cfg(test)]
pub(crate) fn with_checksum(key: &[u8], mut bytes: Vec<u8>) -> Vec<u8> {
    let sum = checksum(key, &bytes[PRESENT_AT..]);
    bytes[CHECKSUM_AT..PRESENT_AT].copy_from_slice(&sum.to_le_bytes());
    bytes
}

/// A chunk in the layout of graph format 14, which carried no checksum.
#[cfg(test)]
pub(crate) fn unsealed(key: &[u8], records: &[(usize, Vec<u8>)]) -> Vec<u8> {
    let mut unsealed = vec![UNSEALED_LAYOUT];
    unsealed.extend_from_slice(&in_records_layout(key, records)[PRESENT_AT..]);
    unsealed
}

/// The slots and records a stored chunk of any layout holds.
#[cfg(test)]
pub(crate) fn stored_records(key: &[u8], bytes: &[u8]) -> Vec<(usize, Vec<u8>)> {
    match bytes.first() {
        Some(&LAYOUT_VERSION) => ChunkRef::parse(key, bytes)
            .unwrap()
            .entries()
            .collect::<Result<_, _>>()
            .unwrap(),
        Some(&layout) => {
            let at = if layout == RECORDS_LAYOUT {
                PRESENT_AT
            } else {
                1
            };
            let (present, records) = present_and_rest(&bytes[at..]).unwrap();
            framed(present, records)
                .map(|entry| entry.map(|(slot, record)| (slot, record.to_vec())))
                .collect::<Result<_, _>>()
                .unwrap()
        }
        None => Vec::new(),
    }
}

#[derive(Clone, Copy, Debug)]
struct Packed<'a> {
    base: u64,
    width: u8,
    bits: &'a [u8],
}

fn put_unsigned(out: &mut Vec<u8>, values: impl Iterator<Item = u64> + Clone) {
    let least = values.clone().min().unwrap_or(0);
    put_packed(out, least, values.map(|value| value - least));
}

fn put_signed(out: &mut Vec<u8>, values: impl Iterator<Item = i64> + Clone) {
    let least = values.clone().min().unwrap_or(0);
    put_packed(
        out,
        zigzag(least),
        values.map(|value| value.wrapping_sub(least) as u64),
    );
}

fn put_packed(out: &mut Vec<u8>, base: u64, distances: impl Iterator<Item = u64> + Clone) {
    let spread = distances.clone().fold(0, u64::max);
    let width = (u64::BITS - spread.leading_zeros()) as u8;
    put_varint_u64(out, base);
    out.push(width);
    if width == 0 {
        return;
    }
    let mut pending = 0u128;
    let mut bits = 0;
    for distance in distances {
        pending |= u128::from(distance) << bits;
        bits += u32::from(width);
        while bits >= 8 {
            out.push(pending as u8);
            pending >>= 8;
            bits -= 8;
        }
    }
    if bits > 0 {
        out.push(pending as u8);
    }
}

fn packed_bytes(count: usize, width: u8) -> usize {
    (count * usize::from(width)).div_ceil(8)
}

impl<'a> Packed<'a> {
    fn read(reader: &mut Reader<'a>, count: usize) -> Result<Self, GraphError> {
        let base = reader.varint_u64()?;
        let width = reader.u8()?;
        if width > MAX_WIDTH {
            return Err(BAD_COLUMNS);
        }
        let bits = reader.take(packed_bytes(count, width))?;
        Ok(Self { base, width, bits })
    }

    fn distance(&self, index: usize) -> u64 {
        let width = usize::from(self.width);
        let start = index * width;
        let mut window = 0u128;
        for (shift, byte) in self.bits[start / 8..(start + width).div_ceil(8)]
            .iter()
            .enumerate()
        {
            window |= u128::from(*byte) << (8 * shift);
        }
        let mask = (1u128 << width) - 1;
        ((window >> (start % 8)) & mask) as u64
    }

    fn unsigned(&self, index: usize) -> Result<u64, GraphError> {
        self.base
            .checked_add(self.distance(index))
            .ok_or(BAD_COLUMNS)
    }

    fn signed(&self, index: usize) -> i64 {
        unzigzag(self.base).wrapping_add(self.distance(index) as i64)
    }
}

#[derive(Clone, Copy, Debug)]
struct Column<'a> {
    key: u64,
    holders: u32,
    values: Values<'a>,
}

#[derive(Clone, Copy, Debug)]
enum Values<'a> {
    Integers(Packed<'a>),
    Tagged(&'a [u8]),
}

impl<'a> Column<'a> {
    fn read(reader: &mut Reader<'a>, present: u32) -> Result<Self, GraphError> {
        let key = reader.varint_u64()?;
        let mut holders = [0u8; 4];
        holders.copy_from_slice(reader.take(4)?);
        let holders = u32::from_le_bytes(holders);
        if holders == 0 || holders & !present != 0 {
            return Err(BAD_COLUMNS);
        }
        let values = match reader.u8()? {
            INTEGERS => Values::Integers(Packed::read(reader, holders.count_ones() as usize)?),
            TAGGED => {
                let length = reader.varint()?;
                Values::Tagged(reader.take(length)?)
            }
            _ => return Err(BAD_COLUMNS),
        };
        Ok(Self {
            key,
            holders,
            values,
        })
    }

    fn check(&self) -> Result<(), GraphError> {
        if let Values::Tagged(tagged) = self.values {
            let mut values = Reader::new(tagged);
            for _ in 0..self.holders.count_ones() {
                values.skip_value()?;
            }
            values.finish()?;
        }
        Ok(())
    }

    fn put_value(&self, slot: usize, out: &mut Vec<u8>) -> Result<(), GraphError> {
        let index = (self.holders & ((1 << slot) - 1)).count_ones() as usize;
        match self.values {
            Values::Integers(packed) => {
                PropertyValue::Integer(packed.signed(index)).encode_compact_into(out);
            }
            Values::Tagged(tagged) => {
                let mut reader = Reader::new(tagged);
                for _ in 0..index {
                    reader.skip_value()?;
                }
                let rest = reader.remaining();
                reader.skip_value()?;
                let end = tagged.len() - reader.remaining();
                out.extend_from_slice(&tagged[tagged.len() - rest..end]);
            }
        }
        Ok(())
    }
}

/// A stored chunk read in place: a layout byte, the little-endian CRC-32 of its key and of the
/// bytes after it, a little-endian bitmap of the slots that hold a relationship, the types,
/// sources, and targets of those relationships in slot order as three packed columns, and then
/// the count of property keys the chunk's relationships hold and, for each key in ascending
/// order, its token, a little-endian bitmap of the slots whose relationship holds it, a kind
/// byte, and its values: packed when all are integers, tagged after their length in bytes
/// otherwise. A packed column holds the least value as a varint, zigzag encoded for signed
/// integers, the width in bits as a byte, and each value's distance from the least in that many
/// bits, the first in the lowest bits of the first byte.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ChunkRef<'a> {
    present: u32,
    rel_type: Packed<'a>,
    source: Packed<'a>,
    target: Packed<'a>,
    keys: usize,
    columns: &'a [u8],
}

impl<'a> ChunkRef<'a> {
    /// The chunk stored under `key`, checked against its checksum and for columns that frame
    /// its relationships.
    pub(crate) fn parse(key: &[u8], bytes: &'a [u8]) -> Result<Self, GraphError> {
        if bytes.first() != Some(&LAYOUT_VERSION) {
            return Err(UNKNOWN_LAYOUT);
        }
        sealed(key, bytes)?;
        let (present, rest) = present_and_rest(&bytes[PRESENT_AT..])?;
        let count = present.count_ones() as usize;
        let mut reader = Reader::new(rest);
        let rel_type = Packed::read(&mut reader, count)?;
        let source = Packed::read(&mut reader, count)?;
        let target = Packed::read(&mut reader, count)?;
        for index in 0..count {
            if u32::try_from(rel_type.unsigned(index)?).is_err() {
                return Err(BAD_COLUMNS);
            }
            source.unsigned(index)?;
            target.unsigned(index)?;
        }
        let keys = reader.varint()?;
        let columns = &rest[rest.len() - reader.remaining()..];
        let mut previous = None;
        for _ in 0..keys {
            let column = Column::read(&mut reader, present)?;
            if previous.is_some_and(|previous| previous >= column.key)
                || u32::try_from(column.key).is_err()
            {
                return Err(BAD_COLUMNS);
            }
            column.check()?;
            previous = Some(column.key);
        }
        reader.finish()?;
        Ok(Self {
            present,
            rel_type,
            source,
            target,
            keys,
            columns,
        })
    }

    fn columns(&self) -> impl Iterator<Item = Column<'a>> + use<'a> {
        let mut reader = Reader::new(self.columns);
        let present = self.present;
        (0..self.keys).filter_map(move |_| Column::read(&mut reader, present).ok())
    }

    /// The record of the relationship in `slot`, in the encoding of
    /// [`RelationshipRecord::encode`].
    pub(crate) fn get(&self, slot: usize) -> Result<Option<Vec<u8>>, GraphError> {
        let bit = 1u32 << slot;
        if self.present & bit == 0 {
            return Ok(None);
        }
        let index = (self.present & (bit - 1)).count_ones() as usize;
        let mut out = Vec::with_capacity(16);
        put_varint_u64(&mut out, self.rel_type.unsigned(index)?);
        put_varint_u64(&mut out, self.source.unsigned(index)?);
        put_varint_u64(&mut out, self.target.unsigned(index)?);
        let held = self
            .columns()
            .filter(|column| column.holders & bit != 0)
            .count();
        put_varint(&mut out, held);
        for column in self.columns().filter(|column| column.holders & bit != 0) {
            put_varint_u64(&mut out, column.key);
            column.put_value(slot, &mut out)?;
        }
        Ok(Some(out))
    }

    /// Every slot that holds a relationship, ascending, with its record.
    pub(crate) fn entries(
        &self,
    ) -> impl Iterator<Item = Result<(usize, Vec<u8>), GraphError>> + use<'a> {
        let chunk = *self;
        let mut present = self.present;
        std::iter::from_fn(move || {
            if present == 0 {
                return None;
            }
            let slot = present.trailing_zeros() as usize;
            present &= present - 1;
            Some(
                chunk
                    .get(slot)
                    .map(|record| (slot, record.unwrap_or_default())),
            )
        })
    }
}

struct Property<'a> {
    key: u64,
    slot: u8,
    integer: Option<i64>,
    value: &'a [u8],
}

/// A chunk held decoded while a write transaction changes it, each record in the encoding of
/// [`RelationshipRecord::encode`].
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

    /// The form of the chunk stored under `key`, or `None` when it holds no relationship. Fails
    /// when a record it holds does not frame.
    pub(crate) fn encode(&self, key: &[u8]) -> Result<Option<Vec<u8>>, GraphError> {
        if self.records.is_empty() {
            return Ok(None);
        }
        let mut present = 0u32;
        let mut ends = Vec::with_capacity(self.records.len());
        let mut properties: Vec<Property<'_>> = Vec::new();
        for (slot, record) in &self.records {
            present |= 1 << slot;
            let mut reader = Reader::new(record);
            ends.push([
                reader.varint_u64()?,
                reader.varint_u64()?,
                reader.varint_u64()?,
            ]);
            for _ in 0..reader.varint()? {
                let key = reader.varint_u64()?;
                let start = record.len() - reader.remaining();
                let integer = reader.integer_or_skip()?;
                let value = &record[start..record.len() - reader.remaining()];
                properties.push(Property {
                    key,
                    slot: *slot,
                    integer,
                    value,
                });
            }
            reader.finish()?;
        }
        properties.sort_by_key(|property| property.key);
        let mut out = Vec::with_capacity(PRESENT_AT + 4 + self.bytes);
        out.push(LAYOUT_VERSION);
        out.extend_from_slice(&[0; 8]);
        for field in 0..3 {
            put_unsigned(&mut out, ends.iter().map(|ends| ends[field]));
        }
        let columns = properties.chunk_by(|left, right| left.key == right.key);
        put_varint(&mut out, columns.clone().count());
        for column in columns {
            put_varint_u64(&mut out, column[0].key);
            let holders = column
                .iter()
                .fold(0u32, |holders, property| holders | 1 << property.slot);
            out.extend_from_slice(&holders.to_le_bytes());
            if column.iter().all(|property| property.integer.is_some()) {
                out.push(INTEGERS);
                put_signed(
                    &mut out,
                    column.iter().filter_map(|property| property.integer),
                );
            } else {
                out.push(TAGGED);
                let mut values = Vec::new();
                for property in column {
                    match property.integer {
                        Some(integer) => {
                            PropertyValue::Integer(integer).encode_compact_into(&mut values);
                        }
                        None => values.extend_from_slice(property.value),
                    }
                }
                put_varint(&mut out, values.len());
                out.extend_from_slice(&values);
            }
        }
        out[PRESENT_AT..PRESENT_AT + 4].copy_from_slice(&present.to_le_bytes());
        let sum = checksum(key, &out[PRESENT_AT..]);
        out[CHECKSUM_AT..PRESENT_AT].copy_from_slice(&sum.to_le_bytes());
        Ok(Some(out))
    }

    fn position(&self, slot: usize) -> Result<usize, usize> {
        self.records
            .binary_search_by_key(&slot, |(held, _)| usize::from(*held))
    }
}

#[cfg(test)]
#[path = "relationships.test.rs"]
mod tests;
