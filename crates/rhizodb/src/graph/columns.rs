use std::collections::BTreeMap;

use crate::value::DecodeError;
use crate::value::PropertyValue;
use crate::value::Reader;
use crate::value::TAG_BOOLEAN;
use crate::value::TAG_FLOAT;
use crate::value::TAG_INTEGER;
use crate::value::TAG_STRING;
use crate::value::put_varint;

use super::GraphError;
use super::ids::PropertyKey;
use super::ids::Token;
use super::meta::SLOTS;

const KEY: usize = 16;
const LAYOUT_VERSION: u8 = 1;
const HEAD_CHECKSUM: usize = 1;
const KIND_AT: usize = 5;
const GROUP_AT: usize = 6;
const TOKEN_AT: usize = 14;
const COUNT_AT: usize = 18;
const HEAP_AT: usize = 20;
const BITMAP_AT: usize = 24;
const WORDS: usize = SLOTS / 64;
const BLOCKS_AT: usize = BITMAP_AT + 8 * WORDS;
const BLOCK: usize = 64;
const PRESENT: u8 = 1;
const ABSENT: u8 = 0;
/// The most deltas a column keeps beside its chunk. A commit that would leave more rewrites
/// the chunk and drops them.
pub(crate) const MAX_DELTAS: usize = 64;

const CHECKSUM_MISMATCH: GraphError =
    GraphError::Corrupt("a property column entry does not match its checksum");
const DELTA_WITHOUT_CHUNK: GraphError = GraphError::Corrupt("a property column delta has no chunk");
const VALUE_OUTSIDE: GraphError =
    GraphError::Corrupt("a property column value lies outside its chunk");

/// A property column: the row group and the property key.
pub(crate) type ColumnId = (u64, Token<PropertyKey>);

fn key(group: u64, token: Token<PropertyKey>, part: u32) -> [u8; KEY] {
    let mut key = [0u8; KEY];
    key[..8].copy_from_slice(&group.to_be_bytes());
    key[8..12].copy_from_slice(&token.id().to_be_bytes());
    key[12..].copy_from_slice(&part.to_be_bytes());
    key
}

/// The key of a column's chunk: the group, the property key, and part 0.
pub(crate) fn chunk_key(group: u64, token: Token<PropertyKey>) -> [u8; KEY] {
    key(group, token, 0)
}

/// The key of the delta that amends a column's chunk at `slot`: part `1 + slot`, so a column's
/// deltas follow its chunk in slot order.
pub(crate) fn delta_key(group: u64, token: Token<PropertyKey>, slot: usize) -> [u8; KEY] {
    key(group, token, 1 + slot as u32)
}

/// The prefix of every entry of one column.
/// The start, inclusive, and the end, exclusive, of the keys of every delta of a column.
pub(crate) fn delta_range(group: u64, token: Token<PropertyKey>) -> ([u8; KEY], [u8; KEY]) {
    (key(group, token, 1), key(group, token, SLOTS as u32 + 1))
}

pub(crate) fn column_prefix(group: u64, token: Token<PropertyKey>) -> [u8; 12] {
    let mut prefix = [0u8; 12];
    prefix.copy_from_slice(&chunk_key(group, token)[..12]);
    prefix
}

/// The prefix of every column entry of one group.
pub(crate) fn group_prefix(group: u64) -> [u8; 8] {
    group.to_be_bytes()
}

/// What a column key names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Part {
    Chunk,
    Delta(usize),
}

/// The group, property key, and part a column key names.
pub(crate) fn parse_key(key: &[u8]) -> Result<(u64, Token<PropertyKey>, Part), GraphError> {
    let key = <[u8; KEY]>::try_from(key).map_err(|_| DecodeError::UnexpectedEnd)?;
    let mut group = [0u8; 8];
    group.copy_from_slice(&key[..8]);
    let token = Token::new(u32::from_be_bytes([key[8], key[9], key[10], key[11]]));
    let part = u32::from_be_bytes([key[12], key[13], key[14], key[15]]) as usize;
    let part = match part {
        0 => Part::Chunk,
        1..=SLOTS => Part::Delta(part - 1),
        _ => return Err(GraphError::Corrupt("a property column key names no part")),
    };
    Ok((u64::from_be_bytes(group), token, part))
}

/// How a chunk stores its values: one type for every value, or each value encoded with its tag
/// when the values differ in type or are of another type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Bool,
    Int,
    Float,
    Str,
    Variant,
}

impl Kind {
    fn tag(self) -> u8 {
        match self {
            Kind::Bool => 0,
            Kind::Int => 1,
            Kind::Float => 2,
            Kind::Str => 3,
            Kind::Variant => 4,
        }
    }

    fn from_tag(tag: u8) -> Option<Self> {
        Some(match tag {
            0 => Kind::Bool,
            1 => Kind::Int,
            2 => Kind::Float,
            3 => Kind::Str,
            4 => Kind::Variant,
            _ => return None,
        })
    }

    fn varying(self) -> bool {
        matches!(self, Kind::Str | Kind::Variant)
    }

    fn body(self, count: usize, heap: usize) -> Option<usize> {
        match self {
            Kind::Bool => Some(count.div_ceil(8)),
            Kind::Int | Kind::Float => count.checked_mul(8),
            Kind::Str | Kind::Variant => count.checked_mul(4)?.checked_add(heap),
        }
    }
}

/// One stored value, read in place where its chunk allows.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Cell<'a> {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(&'a str),
    /// A value of another type in its tagged encoding.
    Encoded(&'a [u8]),
    /// A value of another type, not encoded yet.
    Value(&'a PropertyValue),
}

impl<'a> Cell<'a> {
    pub(crate) fn of(value: &'a PropertyValue) -> Self {
        match value {
            PropertyValue::Boolean(value) => Cell::Bool(*value),
            PropertyValue::Integer(value) => Cell::Int(*value),
            PropertyValue::Float(value) => Cell::Float(*value),
            PropertyValue::String(value) => Cell::Str(value),
            other => Cell::Value(other),
        }
    }

    fn kind(&self) -> Kind {
        match self {
            Cell::Bool(_) => Kind::Bool,
            Cell::Int(_) => Kind::Int,
            Cell::Float(_) => Kind::Float,
            Cell::Str(_) => Kind::Str,
            Cell::Encoded(_) | Cell::Value(_) => Kind::Variant,
        }
    }

    fn encode_tagged(&self, out: &mut Vec<u8>) {
        match self {
            Cell::Bool(value) => {
                out.push(TAG_BOOLEAN);
                out.push(u8::from(*value));
            }
            Cell::Int(value) => {
                out.push(TAG_INTEGER);
                out.extend_from_slice(&value.to_be_bytes());
            }
            Cell::Float(value) => {
                out.push(TAG_FLOAT);
                out.extend_from_slice(&value.to_bits().to_be_bytes());
            }
            Cell::Str(value) => {
                out.push(TAG_STRING);
                put_varint(out, value.len());
                out.extend_from_slice(value.as_bytes());
            }
            Cell::Encoded(bytes) => out.extend_from_slice(bytes),
            Cell::Value(value) => value.encode_into(out),
        }
    }

    pub(crate) fn to_value(self) -> Result<PropertyValue, GraphError> {
        Ok(match self {
            Cell::Bool(value) => PropertyValue::Boolean(value),
            Cell::Int(value) => PropertyValue::Integer(value),
            Cell::Float(value) => PropertyValue::Float(value),
            Cell::Str(value) => PropertyValue::String(value.to_owned()),
            Cell::Encoded(bytes) => {
                let mut reader = Reader::new(bytes);
                let value = reader.value()?;
                reader.finish()?;
                value
            }
            Cell::Value(value) => value.clone(),
        })
    }

    /// Whether both cells hold the same value bit for bit, so `-0.0` differs from `0.0`.
    pub(crate) fn same(&self, other: &Cell<'_>) -> bool {
        match (self, other) {
            (Cell::Bool(left), Cell::Bool(right)) => left == right,
            (Cell::Int(left), Cell::Int(right)) => left == right,
            (Cell::Float(left), Cell::Float(right)) => left.to_bits() == right.to_bits(),
            (Cell::Str(left), Cell::Str(right)) => left == right,
            (left, right) if left.kind() == Kind::Variant && right.kind() == Kind::Variant => {
                let mut left_bytes = Vec::new();
                let mut right_bytes = Vec::new();
                left.encode_tagged(&mut left_bytes);
                right.encode_tagged(&mut right_bytes);
                left_bytes == right_bytes
            }
            _ => false,
        }
    }
}

/// Whether two stored states of a slot are the same, absent included.
pub(crate) fn same_state(left: Option<Cell<'_>>, right: Option<Cell<'_>>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => left.same(&right),
        _ => false,
    }
}

fn classify(bytes: &[u8]) -> Result<Cell<'_>, GraphError> {
    let mut reader = Reader::new(bytes);
    let cell = match bytes.first() {
        Some(&TAG_BOOLEAN) => {
            reader.u8()?;
            match reader.u8()? {
                0 => Cell::Bool(false),
                1 => Cell::Bool(true),
                other => return Err(DecodeError::InvalidBoolean(other).into()),
            }
        }
        Some(&TAG_INTEGER) => {
            reader.u8()?;
            Cell::Int(reader.u64()? as i64)
        }
        Some(&TAG_FLOAT) => {
            reader.u8()?;
            Cell::Float(f64::from_bits(reader.u64()?))
        }
        Some(&TAG_STRING) => {
            reader.u8()?;
            let length = reader.varint()?;
            let text = reader.take(length)?;
            Cell::Str(std::str::from_utf8(text).map_err(|_| DecodeError::InvalidUtf8)?)
        }
        _ => {
            reader.skip_value()?;
            Cell::Encoded(bytes)
        }
    };
    reader.finish()?;
    Ok(cell)
}

fn crc(parts: &[&[u8]]) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize()
}

fn u32_le(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn ones(words: impl Iterator<Item = u64>) -> impl Iterator<Item = usize> {
    words.enumerate().flat_map(|(index, word)| {
        let mut rest = word;
        std::iter::from_fn(move || {
            if rest == 0 {
                return None;
            }
            let bit = rest.trailing_zeros() as usize;
            rest &= rest - 1;
            Some(index * 64 + bit)
        })
    })
}

/// The values of one column, as the chunk that stores them: the slots that hold a value,
/// ascending, each with its value. `None` when no slot holds one, since such a column has no
/// chunk.
pub(crate) fn encode_chunk(
    group: u64,
    token: Token<PropertyKey>,
    cells: &[(usize, Cell<'_>)],
) -> Result<Option<Vec<u8>>, GraphError> {
    let Some(kind) = cells
        .iter()
        .map(|(_, cell)| cell.kind())
        .reduce(|left, right| if left == right { left } else { Kind::Variant })
    else {
        return Ok(None);
    };
    let count = cells.len();
    let blocks = count.div_ceil(BLOCK);
    let mut words = [0u64; WORDS];
    for (slot, _) in cells {
        words[slot / 64] |= 1 << (slot % 64);
    }
    let mut ends: Vec<u8> = Vec::new();
    let mut body: Vec<u8> = Vec::new();
    match kind {
        Kind::Bool => {
            body.resize(count.div_ceil(8), 0);
            for (index, (_, cell)) in cells.iter().enumerate() {
                if matches!(cell, Cell::Bool(true)) {
                    body[index / 8] |= 1 << (index % 8);
                }
            }
        }
        Kind::Int | Kind::Float => {
            body.reserve(8 * count);
            for (_, cell) in cells {
                match cell {
                    Cell::Int(value) => body.extend_from_slice(&value.to_le_bytes()),
                    Cell::Float(value) => body.extend_from_slice(&value.to_bits().to_le_bytes()),
                    _ => {}
                }
            }
        }
        Kind::Str | Kind::Variant => {
            ends.reserve(4 * count);
            for (_, cell) in cells {
                match (kind, cell) {
                    (Kind::Str, Cell::Str(text)) => body.extend_from_slice(text.as_bytes()),
                    _ => cell.encode_tagged(&mut body),
                }
                let end = u32::try_from(body.len()).map_err(|_| DecodeError::LengthOverflow)?;
                ends.extend_from_slice(&end.to_le_bytes());
            }
        }
    }
    let heap = if kind.varying() { body.len() } else { 0 };
    let heap = u32::try_from(heap).map_err(|_| DecodeError::LengthOverflow)?;
    let head_end = BLOCKS_AT + 4 * blocks;
    let mut out = Vec::with_capacity(head_end + ends.len() + body.len());
    out.push(LAYOUT_VERSION);
    out.extend_from_slice(&[0; 4]);
    out.push(kind.tag());
    out.extend_from_slice(&group.to_be_bytes());
    out.extend_from_slice(&token.id().to_be_bytes());
    out.extend_from_slice(&(count as u16).to_be_bytes());
    out.extend_from_slice(&heap.to_be_bytes());
    for word in words {
        out.extend_from_slice(&word.to_le_bytes());
    }
    out.resize(head_end, 0);
    out.extend_from_slice(&ends);
    out.extend_from_slice(&body);
    let layout = Layout::of(&out)?;
    for block in 0..blocks {
        let sum = layout.block_checksum(&out, block)?;
        let at = BLOCKS_AT + 4 * block;
        out[at..at + 4].copy_from_slice(&sum.to_le_bytes());
    }
    let head = crc(&[&out[KIND_AT..head_end]]);
    out[HEAD_CHECKSUM..KIND_AT].copy_from_slice(&head.to_le_bytes());
    Ok(Some(out))
}

#[derive(Clone, Copy, Debug)]
struct Layout {
    kind: Kind,
    count: usize,
    head_end: usize,
    values: usize,
    heap: usize,
}

impl Layout {
    fn of(bytes: &[u8]) -> Result<Self, GraphError> {
        if bytes.len() < BLOCKS_AT || bytes[0] != LAYOUT_VERSION {
            return Err(GraphError::Corrupt(
                "a property column chunk has an unknown layout",
            ));
        }
        let kind = Kind::from_tag(bytes[KIND_AT]).ok_or(GraphError::Corrupt(
            "a property column chunk has an unknown type",
        ))?;
        let count = usize::from(u16::from_be_bytes([bytes[COUNT_AT], bytes[COUNT_AT + 1]]));
        let heap = u32::from_be_bytes([
            bytes[HEAP_AT],
            bytes[HEAP_AT + 1],
            bytes[HEAP_AT + 2],
            bytes[HEAP_AT + 3],
        ]) as usize;
        if count == 0 || count > SLOTS || (heap != 0 && !kind.varying()) {
            return Err(GraphError::Corrupt(
                "a property column chunk has an unknown layout",
            ));
        }
        let head_end = BLOCKS_AT + 4 * count.div_ceil(BLOCK);
        let end = kind
            .body(count, heap)
            .and_then(|body| body.checked_add(head_end))
            .ok_or(DecodeError::LengthOverflow)?;
        match end.cmp(&bytes.len()) {
            std::cmp::Ordering::Less => Err(DecodeError::TrailingBytes.into()),
            std::cmp::Ordering::Greater => Err(DecodeError::UnexpectedEnd.into()),
            std::cmp::Ordering::Equal => Ok(Self {
                kind,
                count,
                head_end,
                values: head_end,
                heap: head_end + if kind.varying() { 4 * count } else { 0 },
            }),
        }
    }

    fn block_values(&self, block: usize) -> std::ops::Range<usize> {
        let start = block * BLOCK;
        start..(start + BLOCK).min(self.count)
    }

    fn end_of(&self, bytes: &[u8], index: usize) -> usize {
        u32_le(bytes, self.values + 4 * index) as usize
    }

    fn block_checksum(&self, bytes: &[u8], block: usize) -> Result<u32, GraphError> {
        let values = self.block_values(block);
        Ok(match self.kind {
            Kind::Bool => {
                crc(
                    &[
                        &bytes
                            [self.values + values.start / 8..self.values + values.end.div_ceil(8)],
                    ],
                )
            }
            Kind::Int | Kind::Float => {
                crc(&[&bytes[self.values + 8 * values.start..self.values + 8 * values.end]])
            }
            Kind::Str | Kind::Variant => {
                let first = values.start.saturating_sub(1);
                let ends = &bytes[self.values + 4 * first..self.values + 4 * values.end];
                let start = match values.start {
                    0 => 0,
                    start => self.end_of(bytes, start - 1),
                };
                let end = self.end_of(bytes, values.end - 1);
                let heap = bytes
                    .get(self.heap + start..)
                    .and_then(|rest| rest.get(..end.checked_sub(start)?))
                    .ok_or(VALUE_OUTSIDE)?;
                crc(&[ends, heap])
            }
        })
    }
}

/// A stored chunk read in place. Its head was checked; each block of values is checked when a
/// value in it is read, or all at once by [`ChunkRef::verify`].
#[derive(Clone, Copy, Debug)]
pub(crate) struct ChunkRef<'a> {
    bytes: &'a [u8],
    layout: Layout,
}

impl<'a> ChunkRef<'a> {
    /// The chunk stored under the key of `group` and `token`, with its head checked.
    pub(crate) fn parse(
        group: u64,
        token: Token<PropertyKey>,
        bytes: &'a [u8],
    ) -> Result<Self, GraphError> {
        let layout = Layout::of(bytes)?;
        if crc(&[&bytes[KIND_AT..layout.head_end]]) != u32_le(bytes, HEAD_CHECKSUM) {
            return Err(CHECKSUM_MISMATCH);
        }
        let mut stored = [0u8; 8];
        stored.copy_from_slice(&bytes[GROUP_AT..TOKEN_AT]);
        let stored_token = u32::from_be_bytes([
            bytes[TOKEN_AT],
            bytes[TOKEN_AT + 1],
            bytes[TOKEN_AT + 2],
            bytes[TOKEN_AT + 3],
        ]);
        if u64::from_be_bytes(stored) != group || stored_token != token.id() {
            return Err(GraphError::Corrupt(
                "a property column chunk is stored under the key of another column",
            ));
        }
        let chunk = Self { bytes, layout };
        let present: usize = chunk.words().map(|word| word.count_ones() as usize).sum();
        if present != layout.count {
            return Err(GraphError::Corrupt(
                "a property column chunk counts other values than it holds",
            ));
        }
        Ok(chunk)
    }

    fn word(&self, index: usize) -> u64 {
        let at = BITMAP_AT + 8 * index;
        let mut word = [0u8; 8];
        word.copy_from_slice(&self.bytes[at..at + 8]);
        u64::from_le_bytes(word)
    }

    pub(crate) fn words(&self) -> impl Iterator<Item = u64> + '_ {
        (0..WORDS).map(|index| self.word(index))
    }

    fn index_of(&self, slot: usize) -> Option<usize> {
        let word = self.word(slot / 64);
        let bit = 1u64 << (slot % 64);
        if word & bit == 0 {
            return None;
        }
        let before: u32 = (0..slot / 64)
            .map(|index| self.word(index).count_ones())
            .sum();
        Some(before as usize + (word & (bit - 1)).count_ones() as usize)
    }

    fn verify_block(&self, block: usize) -> Result<(), GraphError> {
        if self.layout.block_checksum(self.bytes, block)?
            != u32_le(self.bytes, BLOCKS_AT + 4 * block)
        {
            return Err(CHECKSUM_MISMATCH);
        }
        if self.layout.kind.varying() {
            let mut start = match block * BLOCK {
                0 => 0,
                first => self.layout.end_of(self.bytes, first - 1),
            };
            for index in self.layout.block_values(block) {
                let end = self.layout.end_of(self.bytes, index);
                if end < start {
                    return Err(VALUE_OUTSIDE);
                }
                start = end;
            }
        }
        Ok(())
    }

    /// Checks every block of values.
    pub(crate) fn verify(&self) -> Result<(), GraphError> {
        (0..self.layout.count.div_ceil(BLOCK)).try_for_each(|block| self.verify_block(block))
    }

    fn cell(&self, index: usize) -> Result<Cell<'a>, GraphError> {
        let layout = self.layout;
        let bytes = self.bytes;
        let fixed = |at: usize| {
            let mut value = [0u8; 8];
            value.copy_from_slice(&bytes[at..at + 8]);
            value
        };
        Ok(match layout.kind {
            Kind::Bool => Cell::Bool(bytes[layout.values + index / 8] & (1 << (index % 8)) != 0),
            Kind::Int => Cell::Int(i64::from_le_bytes(fixed(layout.values + 8 * index))),
            Kind::Float => Cell::Float(f64::from_bits(u64::from_le_bytes(fixed(
                layout.values + 8 * index,
            )))),
            Kind::Str | Kind::Variant => {
                let start = match index {
                    0 => 0,
                    index => layout.end_of(bytes, index - 1),
                };
                let end = layout.end_of(bytes, index);
                let value = bytes
                    .get(layout.heap + start..layout.heap + end)
                    .filter(|_| start <= end)
                    .ok_or(VALUE_OUTSIDE)?;
                if layout.kind == Kind::Str {
                    Cell::Str(std::str::from_utf8(value).map_err(|_| DecodeError::InvalidUtf8)?)
                } else {
                    classify(value)?
                }
            }
        })
    }

    /// The value in `slot`, checking only the block that holds it.
    pub(crate) fn cell_at(&self, slot: usize) -> Result<Option<Cell<'a>>, GraphError> {
        let Some(index) = self.index_of(slot) else {
            return Ok(None);
        };
        self.verify_block(index / BLOCK)?;
        self.cell(index).map(Some)
    }

    /// Every slot that holds a value, ascending, with its value. The chunk must have been
    /// checked with [`ChunkRef::verify`].
    pub(crate) fn cells(&self) -> impl Iterator<Item = Result<(usize, Cell<'a>), GraphError>> + '_ {
        ones(self.words())
            .enumerate()
            .map(|(index, slot)| Ok((slot, self.cell(index)?)))
    }

    /// The presence bitmap and value count, which is all a write keeps of a stored chunk.
    pub(crate) fn head(&self) -> StoredHead {
        let mut words = [0u64; WORDS];
        for (index, word) in words.iter_mut().enumerate() {
            *word = self.word(index);
        }
        StoredHead {
            words,
            count: self.layout.count,
        }
    }
}

/// A stored chunk copied out of storage and checked whole, for a reader that reads many of its
/// values.
#[derive(Debug)]
pub(crate) struct Column {
    bytes: Box<[u8]>,
    layout: Layout,
    ranks: [u16; WORDS],
}

impl Column {
    pub(crate) fn load(
        group: u64,
        token: Token<PropertyKey>,
        bytes: &[u8],
    ) -> Result<Self, GraphError> {
        let chunk = ChunkRef::parse(group, token, bytes)?;
        chunk.verify()?;
        let mut ranks = [0u16; WORDS];
        let mut rank = 0u16;
        for (index, word) in chunk.words().enumerate() {
            ranks[index] = rank;
            rank += word.count_ones() as u16;
        }
        Ok(Self {
            layout: chunk.layout,
            bytes: bytes.into(),
            ranks,
        })
    }

    fn view(&self) -> ChunkRef<'_> {
        ChunkRef {
            bytes: &self.bytes,
            layout: self.layout,
        }
    }

    pub(crate) fn cell_at(&self, slot: usize) -> Result<Option<Cell<'_>>, GraphError> {
        let view = self.view();
        let word = view.word(slot / 64);
        let bit = 1u64 << (slot % 64);
        if word & bit == 0 {
            return Ok(None);
        }
        let index = usize::from(self.ranks[slot / 64]) + (word & (bit - 1)).count_ones() as usize;
        view.cell(index).map(Some)
    }
}

/// The presence bitmap and value count of a stored chunk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StoredHead {
    words: [u64; WORDS],
    count: usize,
}

impl StoredHead {
    fn has(&self, slot: usize) -> bool {
        self.words[slot / 64] & (1 << (slot % 64)) != 0
    }
}

/// The delta stored for one slot: the slot's value now, or none when the slot holds none.
pub(crate) fn encode_delta(
    group: u64,
    token: Token<PropertyKey>,
    slot: usize,
    value: Option<Cell<'_>>,
) -> Vec<u8> {
    let mut bytes = Vec::new();
    match value {
        Some(value) => {
            bytes.push(PRESENT);
            value.encode_tagged(&mut bytes);
        }
        None => bytes.push(ABSENT),
    }
    let sum = crc(&[&delta_key(group, token, slot), &bytes]);
    bytes.extend_from_slice(&sum.to_le_bytes());
    bytes
}

fn checked_delta<'a>(key: &[u8], bytes: &'a [u8]) -> Result<(usize, Option<&'a [u8]>), GraphError> {
    let (_, _, Part::Delta(slot)) = parse_key(key)? else {
        return Err(GraphError::Corrupt("a property column key names no delta"));
    };
    let body = bytes
        .len()
        .checked_sub(4)
        .filter(|body| *body > 0)
        .ok_or(DecodeError::UnexpectedEnd)?;
    if crc(&[key, &bytes[..body]]) != u32_le(bytes, body) {
        return Err(CHECKSUM_MISMATCH);
    }
    match bytes[0] {
        ABSENT if body == 1 => Ok((slot, None)),
        PRESENT if body > 1 => Ok((slot, Some(&bytes[1..body]))),
        PRESENT => Err(DecodeError::UnexpectedEnd.into()),
        _ => Err(GraphError::Corrupt(
            "a property column delta holds no slot state",
        )),
    }
}

/// The slot and value of the delta stored under `key`, which must name a delta, checked against
/// its checksum.
pub(crate) fn parse_delta(
    key: &[u8],
    bytes: &[u8],
) -> Result<(usize, Option<PropertyValue>), GraphError> {
    let (slot, value) = checked_delta(key, bytes)?;
    let value = value
        .map(|value| {
            let mut reader = Reader::new(value);
            let value = reader.value()?;
            reader.finish()?;
            Ok::<PropertyValue, GraphError>(value)
        })
        .transpose()?;
    Ok((slot, value))
}

/// The slot of the delta stored under `key`, and whether the slot holds a value, checked against
/// the delta's checksum without decoding the value.
pub(crate) fn delta_state(key: &[u8], bytes: &[u8]) -> Result<(usize, bool), GraphError> {
    checked_delta(key, bytes).map(|(slot, value)| (slot, value.is_some()))
}

/// One column in a write transaction: the head of its stored chunk, the slots of its stored
/// deltas with whether each holds a value, the slots this transaction changed with their values
/// now, and what commit needs to store the column as a rewritten chunk or as deltas beside the
/// stored one. A slot the transaction did not change reads as stored.
#[derive(Clone, Debug)]
pub(crate) struct ColumnEdit {
    stored: Option<StoredHead>,
    present: usize,
    stored_deltas: Vec<(usize, bool)>,
    current: BTreeMap<usize, Option<PropertyValue>>,
    fresh: usize,
    rewrite: bool,
}

/// What commit writes for one edited column.
#[derive(Debug)]
pub(crate) enum ColumnFlush {
    /// Store the column as a chunk of its stored values, with its stored deltas and then
    /// `values` applied, or delete its chunk when no value is left, and delete the deltas at
    /// `stale`.
    Rewrite {
        values: BTreeMap<usize, Option<PropertyValue>>,
        stale: Vec<usize>,
    },
    /// Keep the chunk. Each touched slot, with whether the chunk holds a value there, gets a
    /// delta, or loses its stored one when its value is the chunk's again.
    Deltas {
        touched: Vec<(usize, bool, Option<PropertyValue>)>,
        stored: Vec<usize>,
    },
}

impl ColumnEdit {
    /// The column as stored: the head of its chunk, if any, and the slots of its deltas with
    /// whether each holds a value. Deltas need a chunk.
    pub(crate) fn load(
        stored: Option<StoredHead>,
        mut deltas: Vec<(usize, bool)>,
    ) -> Result<Self, GraphError> {
        let Some(head) = stored else {
            if !deltas.is_empty() {
                return Err(DELTA_WITHOUT_CHUNK);
            }
            return Ok(Self {
                stored: None,
                present: 0,
                stored_deltas: Vec::new(),
                current: BTreeMap::new(),
                fresh: 0,
                rewrite: true,
            });
        };
        deltas.sort_unstable_by_key(|(slot, _)| *slot);
        if deltas.windows(2).any(|pair| pair[0].0 == pair[1].0) {
            return Err(GraphError::Corrupt(
                "a property column holds two deltas for one slot",
            ));
        }
        let mut present = head.count;
        for (slot, holds) in &deltas {
            present = (present + usize::from(*holds))
                .checked_sub(usize::from(head.has(*slot)))
                .ok_or(GraphError::Corrupt("a property column counts below zero"))?;
        }
        Ok(Self {
            stored: Some(head),
            present,
            stored_deltas: deltas,
            current: BTreeMap::new(),
            fresh: 0,
            rewrite: false,
        })
    }

    fn stored_delta(&self, slot: usize) -> Option<bool> {
        self.stored_deltas
            .binary_search_by_key(&slot, |(held, _)| *held)
            .ok()
            .map(|index| self.stored_deltas[index].1)
    }

    /// The slot's value as the transaction sees it, when the transaction changed it or the
    /// column stores none there: `Some(None)` for no value, `None` to read the stored column.
    pub(crate) fn current(&self, slot: usize) -> Option<Option<&PropertyValue>> {
        match self.current.get(&slot) {
            Some(value) => Some(value.as_ref()),
            None if self.stored_delta(slot).is_none() && !self.chunk_has(slot) => Some(None),
            None => None,
        }
    }

    fn chunk_has(&self, slot: usize) -> bool {
        self.stored.as_ref().is_some_and(|head| head.has(slot))
    }

    fn has(&self, slot: usize) -> bool {
        match self.current.get(&slot) {
            Some(value) => value.is_some(),
            None => self
                .stored_delta(slot)
                .unwrap_or_else(|| self.chunk_has(slot)),
        }
    }

    /// Sets the slot's value, and rewrites the chunk at commit once the column would keep more
    /// than `MAX_DELTAS` deltas.
    pub(crate) fn set(&mut self, slot: usize, value: Option<PropertyValue>) {
        let had = self.has(slot);
        self.present =
            (self.present + usize::from(value.is_some())).saturating_sub(usize::from(had));
        if !self.rewrite && !self.current.contains_key(&slot) {
            if self.stored_delta(slot).is_none() {
                self.fresh += 1;
            }
            self.rewrite = self.stored_deltas.len() + self.fresh > MAX_DELTAS;
        }
        self.current.insert(slot, value);
    }

    pub(crate) fn flush(self) -> ColumnFlush {
        let stored = self.stored_deltas.iter().map(|(slot, _)| *slot).collect();
        if self.rewrite || self.stored.is_none() || self.present == 0 {
            return ColumnFlush::Rewrite {
                values: self.current,
                stale: stored,
            };
        }
        let head = self.stored;
        let touched = self
            .current
            .into_iter()
            .map(|(slot, value)| {
                let had = head.as_ref().is_some_and(|head| head.has(slot));
                (slot, had, value)
            })
            .collect();
        ColumnFlush::Deltas { touched, stored }
    }
}

/// The stored values of a column merged with changes, ascending by slot, as a chunk takes them.
pub(crate) fn merged<'a>(
    stored: impl Iterator<Item = Result<(usize, Cell<'a>), GraphError>>,
    changes: &'a BTreeMap<usize, Option<PropertyValue>>,
) -> Result<Vec<(usize, Cell<'a>)>, GraphError> {
    let mut cells = Vec::new();
    let mut changes = changes.iter().peekable();
    for entry in stored {
        let (slot, cell) = entry?;
        while let Some((changed, value)) = changes.next_if(|(changed, _)| **changed < slot) {
            if let Some(value) = value {
                cells.push((*changed, Cell::of(value)));
            }
        }
        match changes.next_if(|(changed, _)| **changed == slot) {
            Some((_, Some(value))) => cells.push((slot, Cell::of(value))),
            Some((_, None)) => {}
            None => cells.push((slot, cell)),
        }
    }
    for (changed, value) in changes {
        if let Some(value) = value {
            cells.push((*changed, Cell::of(value)));
        }
    }
    Ok(cells)
}

/// A stored column a reader loaded: its chunk and its deltas in slot order.
#[derive(Debug)]
pub(crate) struct Loaded {
    chunk: Column,
    deltas: Vec<(usize, Option<PropertyValue>)>,
}

#[derive(Debug)]
pub(crate) enum Cached {
    /// Read once in place. The next read loads the column.
    Probed,
    /// Loaded, or `None` when the column has no stored entry.
    Loaded(Option<Box<Loaded>>),
}

/// The stored columns one query read so far. A column read once is read in place, so a
/// point lookup copies nothing; a column read again is loaded and checked whole, so a scan
/// reads each value with no storage access.
#[derive(Debug, Default)]
pub(crate) struct ColumnReader {
    columns: Vec<(ColumnId, Cached)>,
    last: usize,
    keys: Vec<(u64, Vec<Token<PropertyKey>>)>,
}

/// A read of one stored column value.
pub(crate) enum ColumnRead<'r> {
    /// The value, as loaded.
    Value(Option<PropertyValue>),
    /// The column was read once in place before, and now needs loading.
    Load(&'r mut Cached),
    /// The column was never read, and is now marked as read once.
    Probe,
}

impl ColumnReader {
    fn position(&self, id: ColumnId) -> Result<usize, usize> {
        match self.columns.get(self.last) {
            Some((held, _)) if *held == id => Ok(self.last),
            _ => self.columns.binary_search_by_key(&id, |(held, _)| *held),
        }
    }

    pub(crate) fn read(&mut self, id: ColumnId, slot: usize) -> Result<ColumnRead<'_>, GraphError> {
        match self.position(id) {
            Ok(index) => {
                self.last = index;
                let cached = &mut self.columns[index].1;
                match cached {
                    Cached::Loaded(None) => Ok(ColumnRead::Value(None)),
                    Cached::Loaded(Some(loaded)) => Ok(ColumnRead::Value(loaded.value(slot)?)),
                    Cached::Probed => Ok(ColumnRead::Load(cached)),
                }
            }
            Err(index) => {
                self.columns.insert(index, (id, Cached::Probed));
                self.last = index;
                Ok(ColumnRead::Probe)
            }
        }
    }

    /// The property keys with a stored column in `group`, if read before.
    pub(crate) fn keys(&self, group: u64) -> Option<&[Token<PropertyKey>]> {
        self.keys
            .binary_search_by_key(&group, |(held, _)| *held)
            .ok()
            .map(|index| self.keys[index].1.as_slice())
    }

    pub(crate) fn insert_keys(&mut self, group: u64, keys: Vec<Token<PropertyKey>>) {
        if let Err(index) = self.keys.binary_search_by_key(&group, |(held, _)| *held) {
            self.keys.insert(index, (group, keys));
        }
    }
}

impl Cached {
    /// Stores the loaded column and returns the value of `slot`.
    pub(crate) fn fill(
        &mut self,
        chunk: Option<Column>,
        deltas: Vec<(usize, Option<PropertyValue>)>,
        slot: usize,
    ) -> Result<Option<PropertyValue>, GraphError> {
        let loaded = match chunk {
            Some(chunk) => Some(Box::new(Loaded { chunk, deltas })),
            None if deltas.is_empty() => None,
            None => return Err(DELTA_WITHOUT_CHUNK),
        };
        let value = loaded
            .as_ref()
            .map(|loaded| loaded.value(slot))
            .transpose()?
            .flatten();
        *self = Cached::Loaded(loaded);
        Ok(value)
    }
}

impl Loaded {
    fn value(&self, slot: usize) -> Result<Option<PropertyValue>, GraphError> {
        if !self.deltas.is_empty()
            && let Ok(found) = self.deltas.binary_search_by_key(&slot, |(held, _)| *held)
        {
            return Ok(self.deltas[found].1.clone());
        }
        self.chunk.cell_at(slot)?.map(Cell::to_value).transpose()
    }
}

#[cfg(test)]
#[path = "columns.test.rs"]
mod tests;
