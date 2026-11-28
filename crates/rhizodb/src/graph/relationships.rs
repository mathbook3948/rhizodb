use crate::value::DecodeError;
use crate::value::PropertyValue;
use crate::value::Reader;
use crate::value::put_varint;
use crate::value::put_varint_u64;
use crate::value::unzigzag;
use crate::value::zigzag;

use super::GraphError;
use super::ids::RelationshipId;
use super::ids::Token;
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
/// How many property keys one scan of relationship chunks reads.
pub(crate) const SCANNED_KEYS: usize = 4;

const UNKNOWN_LAYOUT: GraphError =
    GraphError::Corrupt("a relationship chunk has an unknown layout");
const CHECKSUM_MISMATCH: GraphError =
    GraphError::Corrupt("a relationship chunk does not match its checksum");
const EMPTY_CHUNK: GraphError = GraphError::Corrupt("a stored relationship chunk is empty");
fn tagged_at<'a>(ends: Packed<'_>, values: &'a [u8], index: usize) -> Result<&'a [u8], GraphError> {
    let start = match index {
        0 => 0,
        _ => ends.unsigned(index - 1)?,
    };
    usize::try_from(start)
        .ok()
        .zip(usize::try_from(ends.unsigned(index)?).ok())
        .and_then(|(start, end)| values.get(start..end))
        .ok_or_else(bad_columns)
}

/// The slots whose bits `mask` sets, ascending.
pub(crate) fn slots(mut mask: u32) -> impl Iterator<Item = usize> {
    std::iter::from_fn(move || {
        (mask != 0).then(|| {
            let slot = mask.trailing_zeros() as usize;
            mask &= mask - 1;
            slot
        })
    })
}

fn bad_columns() -> GraphError {
    BAD_COLUMNS
}

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
        Some(&LAYOUT_VERSION) => {
            for entry in ChunkRef::parse(key, bytes)?.entries() {
                entry?;
            }
            return Ok(None);
        }
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
            .ok_or_else(bad_columns)
    }

    fn signed(&self, index: usize) -> i64 {
        unzigzag(self.base).wrapping_add(self.distance(index) as i64)
    }

    fn unpack(&self, count: usize) -> [u64; CHUNK_SLOTS] {
        let mut out = [0; CHUNK_SLOTS];
        let width = usize::from(self.width);
        if width == 0 {
            return out;
        }
        if width + 7 > 64 {
            for (index, value) in out.iter_mut().take(count).enumerate() {
                *value = self.distance(index);
            }
            return out;
        }
        let mut padded = [0u8; CHUNK_SLOTS * 8 + 8];
        let bits = &self.bits[..self.bits.len().min(CHUNK_SLOTS * 8)];
        padded[..bits.len()].copy_from_slice(bits);
        let mask = u64::MAX >> (64 - width);
        for (index, value) in out.iter_mut().take(count).enumerate() {
            let start = index * width;
            let mut word = [0u8; 8];
            word.copy_from_slice(&padded[start / 8..start / 8 + 8]);
            *value = (u64::from_le_bytes(word) >> (start % 8)) & mask;
        }
        out
    }

    fn unsigned_all(&self, count: usize) -> Result<[u64; CHUNK_SLOTS], GraphError> {
        let mut out = self.unpack(count);
        for value in out.iter_mut().take(count) {
            *value = self.base.checked_add(*value).ok_or_else(bad_columns)?;
        }
        Ok(out)
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
    Tagged { ends: Packed<'a>, values: &'a [u8] },
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
                let count = holders.count_ones() as usize;
                let ends = Packed::read(reader, count)?;
                let length = reader.varint()?;
                let values = reader.take(length)?;
                if ends.unsigned(count - 1)? != length as u64 {
                    return Err(BAD_COLUMNS);
                }
                Values::Tagged { ends, values }
            }
            _ => return Err(BAD_COLUMNS),
        };
        Ok(Self {
            key,
            holders,
            values,
        })
    }

    fn put_value(&self, slot: usize, out: &mut Vec<u8>) -> Result<(), GraphError> {
        let index = (self.holders & ((1 << slot) - 1)).count_ones() as usize;
        self.put_nth(index, out)
    }

    fn value(&self, slot: usize) -> Result<PropertyValue, GraphError> {
        let index = (self.holders & ((1 << slot) - 1)).count_ones() as usize;
        match self.values {
            Values::Integers(packed) => Ok(PropertyValue::Integer(packed.signed(index))),
            Values::Tagged { ends, values } => {
                let mut reader = Reader::new(tagged_at(ends, values, index)?);
                let value = reader.value()?;
                reader.finish()?;
                Ok(value)
            }
        }
    }

    fn put_nth(&self, index: usize, out: &mut Vec<u8>) -> Result<(), GraphError> {
        match self.values {
            Values::Integers(packed) => {
                PropertyValue::Integer(packed.signed(index)).encode_compact_into(out);
            }
            Values::Tagged { ends, values } => {
                let value = tagged_at(ends, values, index)?;
                let mut reader = Reader::new(value);
                reader.skip_value()?;
                reader.finish()?;
                out.extend_from_slice(value);
            }
        }
        Ok(())
    }
}

/// The relationships of one chunk that a scan reads, each by its slot.
#[derive(Debug)]
pub(crate) struct Batch {
    /// The slots that hold a relationship of the type scanned.
    pub(crate) present: u32,
    /// The source of each present relationship, when the scan asks for its ends.
    pub(crate) sources: [u64; CHUNK_SLOTS],
    /// The target of each present relationship, when the scan asks for its ends.
    pub(crate) targets: [u64; CHUNK_SLOTS],
    /// One column per key the scan reads, in the order of its keys.
    pub(crate) columns: [BatchColumn; SCANNED_KEYS],
}

/// The values of one property key in a [`Batch`].
#[derive(Debug)]
pub(crate) struct BatchColumn {
    /// The present slots whose relationship holds the key.
    pub(crate) held: u32,
    /// The slots of `held` whose value is an integer.
    pub(crate) integers: u32,
    /// The integer in each slot of `integers`.
    pub(crate) values: [i64; CHUNK_SLOTS],
}

impl Default for Batch {
    fn default() -> Self {
        Self {
            present: 0,
            sources: [0; CHUNK_SLOTS],
            targets: [0; CHUNK_SLOTS],
            columns: [const {
                BatchColumn {
                    held: 0,
                    integers: 0,
                    values: [0; CHUNK_SLOTS],
                }
            }; SCANNED_KEYS],
        }
    }
}

impl BatchColumn {
    fn set(&mut self, slot: usize, value: &PropertyValue) {
        self.held |= 1 << slot;
        if let PropertyValue::Integer(value) = value {
            self.integers |= 1 << slot;
            self.values[slot] = *value;
        }
    }
}

impl Batch {
    /// Fills the batch from the records a write transaction holds for one chunk.
    pub(crate) fn fill_held<'r>(
        &mut self,
        records: impl Iterator<Item = (usize, &'r [u8])>,
        rel_type: Option<u32>,
        keys: &[Option<u32>],
    ) -> Result<(), GraphError> {
        self.present = 0;
        for column in &mut self.columns[..keys.len()] {
            column.held = 0;
            column.integers = 0;
        }
        for (slot, bytes) in records {
            let record = RelationshipRecord::decode(bytes)?;
            if rel_type.is_some_and(|rel_type| rel_type != record.rel_type.id()) {
                continue;
            }
            self.present |= 1 << slot;
            self.sources[slot] = record.source.0;
            self.targets[slot] = record.target.0;
            for (column, key) in self.columns.iter_mut().zip(keys) {
                if let Some(value) = key.and_then(|key| record.properties.get(&Token::new(key))) {
                    column.set(slot, value);
                }
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
/// byte, and its values: packed when all are integers, and otherwise each in its tagged
/// encoding, after a packed column of the offset each value ends at and the length of all of
/// them in bytes. A packed column holds the least value as a varint, zigzag encoded for signed
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
    /// The chunk stored under `key`, checked against its checksum. The columns of its properties
    /// are checked as a read walks them, and [`ChunkRef::entries`] walks every value.
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
        let keys = reader.varint()?;
        let columns = &rest[rest.len() - reader.remaining()..];
        Ok(Self {
            present,
            rel_type,
            source,
            target,
            keys,
            columns,
        })
    }

    fn put_ends(&self, index: usize, out: &mut Vec<u8>) -> Result<(), GraphError> {
        let rel_type = self.rel_type.unsigned(index)?;
        if u32::try_from(rel_type).is_err() {
            return Err(BAD_COLUMNS);
        }
        put_varint_u64(out, rel_type);
        put_varint_u64(out, self.source.unsigned(index)?);
        put_varint_u64(out, self.target.unsigned(index)?);
        Ok(())
    }

    /// The record of the relationship in `slot`, in the encoding of
    /// [`RelationshipRecord::encode`].
    pub(crate) fn get(&self, slot: usize) -> Result<Option<Vec<u8>>, GraphError> {
        let bit = 1u32 << slot;
        if self.present & bit == 0 {
            return Ok(None);
        }
        let index = (self.present & (bit - 1)).count_ones() as usize;
        let mut out = Vec::with_capacity(24);
        self.put_ends(index, &mut out)?;
        let count_at = out.len();
        out.push(0);
        let mut held = 0;
        let mut columns = self.walk();
        while let Some(column) = columns.next()? {
            if column.holders & bit != 0 {
                held += 1;
                put_varint_u64(&mut out, column.key);
                column.put_value(slot, &mut out)?;
            }
        }
        put_count(&mut out, count_at, held);
        Ok(Some(out))
    }

    /// The type of the relationship in `slot`, or `None` when the slot holds none.
    pub(crate) fn rel_type(&self, slot: usize) -> Result<Option<u32>, GraphError> {
        let Some(index) = self.index(slot) else {
            return Ok(None);
        };
        let rel_type = self.rel_type.unsigned(index)?;
        u32::try_from(rel_type).map(Some).map_err(|_| BAD_COLUMNS)
    }

    /// The value of property `key` of the relationship in `slot`, decoding no other value. The
    /// outer `None` means the slot holds no relationship.
    pub(crate) fn property(
        &self,
        slot: usize,
        key: u32,
    ) -> Result<Option<Option<PropertyValue>>, GraphError> {
        if self.index(slot).is_none() {
            return Ok(None);
        }
        let mut columns = self.walk();
        while let Some(column) = columns.next()? {
            if column.key == u64::from(key) {
                return match column.holders & (1 << slot) {
                    0 => Ok(Some(None)),
                    _ => column.value(slot).map(|value| Some(Some(value))),
                };
            }
        }
        Ok(Some(None))
    }

    /// The property keys of the relationship in `slot`, ascending, decoding no value, or `None`
    /// when the slot holds no relationship.
    pub(crate) fn keys(&self, slot: usize) -> Result<Option<Vec<u32>>, GraphError> {
        if self.index(slot).is_none() {
            return Ok(None);
        }
        let mut keys = Vec::new();
        let mut columns = self.walk();
        while let Some(column) = columns.next()? {
            if column.holders & (1 << slot) != 0 {
                keys.push(column.key as u32);
            }
        }
        Ok(Some(keys))
    }

    /// Fills `batch` with the relationships of type `rel_type`, or of every type, their ends
    /// only when `ends` asks for them, and the values of `keys`, which holds at most
    /// [`SCANNED_KEYS`]. A key of `None` reads as null. Every value read is checked.
    pub(crate) fn fill(
        &self,
        rel_type: Option<u32>,
        ends: bool,
        keys: &[Option<u32>],
        batch: &mut Batch,
    ) -> Result<(), GraphError> {
        let count = self.present.count_ones() as usize;
        let types = match rel_type {
            Some(_) => self.rel_type.unsigned_all(count)?,
            None => [0; CHUNK_SLOTS],
        };
        let (sources, targets) = match ends {
            true => (
                self.source.unsigned_all(count)?,
                self.target.unsigned_all(count)?,
            ),
            false => ([0; CHUNK_SLOTS], [0; CHUNK_SLOTS]),
        };
        let mut wanted = 0;
        for (index, slot) in slots(self.present).enumerate() {
            if rel_type.is_none_or(|rel_type| types[index] == u64::from(rel_type)) {
                wanted |= 1 << slot;
                batch.sources[slot] = sources[index];
                batch.targets[slot] = targets[index];
            }
        }
        batch.present = wanted;
        for column in &mut batch.columns[..keys.len()] {
            column.held = 0;
            column.integers = 0;
        }
        if wanted == 0 {
            return Ok(());
        }
        let mut walk = self.walk();
        while let Some(column) = walk.next()? {
            for (key, held) in keys.iter().zip(&mut batch.columns) {
                if !key.is_some_and(|key| u64::from(key) == column.key) {
                    continue;
                }
                held.held = column.holders & wanted;
                match column.values {
                    Values::Integers(packed) => {
                        held.integers = held.held;
                        let least = unzigzag(packed.base);
                        let distances = packed.unpack(column.holders.count_ones() as usize);
                        for (index, slot) in slots(column.holders).enumerate() {
                            held.values[slot] = least.wrapping_add(distances[index] as i64);
                        }
                    }
                    Values::Tagged { .. } => {
                        for slot in slots(held.held) {
                            if let PropertyValue::Integer(value) = column.value(slot)? {
                                held.integers |= 1 << slot;
                                held.values[slot] = value;
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn index(&self, slot: usize) -> Option<usize> {
        let bit = 1u32 << slot;
        (self.present & bit != 0).then(|| (self.present & (bit - 1)).count_ones() as usize)
    }

    /// Every slot that holds a relationship, ascending, with its record, each value checked as
    /// it is read.
    pub(crate) fn entries(
        &self,
    ) -> impl Iterator<Item = Result<(usize, Vec<u8>), GraphError>> + use<'a> {
        let chunk = *self;
        let mut cursors = self.cursors().map_err(Some);
        let mut present = self.present;
        let mut index = 0;
        let mut failed = false;
        std::iter::from_fn(move || {
            if failed {
                return None;
            }
            let cursors = match &mut cursors {
                Ok(cursors) => cursors,
                Err(error) => {
                    failed = true;
                    return error.take().map(Err);
                }
            };
            if present == 0 {
                failed = true;
                return None;
            }
            let slot = present.trailing_zeros() as usize;
            present &= present - 1;
            let record = chunk.next_record(slot, index, cursors);
            index += 1;
            failed = record.is_err();
            Some(record.map(|record| (slot, record)))
        })
    }

    fn walk(&self) -> Columns<'a> {
        Columns {
            reader: Reader::new(self.columns),
            present: self.present,
            left: self.keys,
            previous: None,
        }
    }

    fn cursors(&self) -> Result<Vec<Cursor<'a>>, GraphError> {
        let mut columns = self.walk();
        let mut cursors = Vec::new();
        while let Some(column) = columns.next()? {
            cursors.push(Cursor { column, next: 0 });
        }
        Ok(cursors)
    }

    fn next_record(
        &self,
        slot: usize,
        index: usize,
        cursors: &mut [Cursor<'a>],
    ) -> Result<Vec<u8>, GraphError> {
        let bit = 1u32 << slot;
        let mut out = Vec::with_capacity(24);
        self.put_ends(index, &mut out)?;
        let count_at = out.len();
        out.push(0);
        let mut held = 0;
        for cursor in cursors.iter_mut() {
            if cursor.column.holders & bit == 0 {
                continue;
            }
            held += 1;
            put_varint_u64(&mut out, cursor.column.key);
            cursor.column.put_nth(cursor.next, &mut out)?;
            cursor.next += 1;
        }
        put_count(&mut out, count_at, held);
        Ok(out)
    }
}

struct Columns<'a> {
    reader: Reader<'a>,
    present: u32,
    left: usize,
    previous: Option<u64>,
}

impl<'a> Columns<'a> {
    fn next(&mut self) -> Result<Option<Column<'a>>, GraphError> {
        if self.left == 0 {
            self.reader.finish()?;
            return Ok(None);
        }
        self.left -= 1;
        let column = Column::read(&mut self.reader, self.present)?;
        if self.previous.is_some_and(|previous| previous >= column.key)
            || u32::try_from(column.key).is_err()
        {
            return Err(BAD_COLUMNS);
        }
        self.previous = Some(column.key);
        Ok(Some(column))
    }
}

struct Cursor<'a> {
    column: Column<'a>,
    next: usize,
}

fn put_count(out: &mut Vec<u8>, at: usize, count: usize) {
    if count < 0x80 {
        out[at] = count as u8;
    } else {
        let mut varint = Vec::new();
        put_varint(&mut varint, count);
        out.splice(at..=at, varint);
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
                let mut ends = Vec::with_capacity(column.len());
                for property in column {
                    match property.integer {
                        Some(integer) => {
                            PropertyValue::Integer(integer).encode_compact_into(&mut values);
                        }
                        None => values.extend_from_slice(property.value),
                    }
                    ends.push(values.len() as u64);
                }
                put_unsigned(&mut out, ends.iter().copied());
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
