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

const LAYOUT_VERSION: u8 = 1;
const DELTA_KIND: u8 = 0;
const SEGMENT_KIND: u8 = 1;
/// The slots of a row group that one segment of a column covers.
pub(crate) const SEGMENT_SLOTS: usize = 64;
/// The segments of one column of a row group.
pub(crate) const SEGMENTS: usize = SLOTS / SEGMENT_SLOTS;
/// The most bytes a segment key takes.
pub(crate) const SEGMENT_KEY: usize = 17;
/// The most bytes a part of a segment takes, unless it holds a single value. A segment that
/// would take more is split into parts that cover equal runs of its slots, so that the storage
/// engine packs four or more parts with their keys into each 4 KiB page instead of giving each
/// one pages of its own.
pub(crate) const PART_BYTES: usize = 992;
/// The length of a delta key.
pub(crate) const DELTA_KEY: usize = 11;
const CHECKSUM_AT: usize = 1;
const CREATED_BYTES: usize = 512;
const KIND_AT: usize = 5;
const SPAN_AT: usize = 6;
const BITMAP_AT: usize = 7;
const KIND_BITS: u8 = 0x07;
const WIDE_OFFSETS: u8 = 0x08;
const PRESENT: u8 = 1;
const ABSENT: u8 = 0;
/// The most deltas a row group keeps beside its segments. A commit that would leave more folds
/// them into the segments.
pub(crate) const MAX_DELTAS: usize = 64;

const CHECKSUM_MISMATCH: GraphError =
    GraphError::Corrupt("a property column entry does not match its checksum");
const UNKNOWN_LAYOUT: GraphError =
    GraphError::Corrupt("a property column segment has an unknown layout");
const VALUE_OUTSIDE: GraphError =
    GraphError::Corrupt("a property column value lies outside its segment");

/// A property column: the row group and the property key.
pub(crate) type ColumnId = (u64, Token<PropertyKey>);

/// The key of a part of a segment: the kind, the group and the property key, each as its
/// length and then its big-endian bytes without leading zeros so that keys sort by number, and
/// then the segment and the part.
#[derive(Clone, Copy)]
pub(crate) struct SegmentKey {
    bytes: [u8; SEGMENT_KEY],
    len: u8,
}

impl SegmentKey {
    fn push(&mut self, byte: u8) {
        if let Some(slot) = self.bytes.get_mut(usize::from(self.len)) {
            *slot = byte;
            self.len += 1;
        }
    }

    fn push_number(&mut self, number: u64) {
        let bytes = number.to_be_bytes();
        let skip = (number.leading_zeros() / 8) as usize;
        self.push((bytes.len() - skip) as u8);
        for byte in &bytes[skip..] {
            self.push(*byte);
        }
    }

    fn cut(mut self, len: usize) -> Self {
        self.len = self.len.min(len as u8);
        self
    }
}

impl std::ops::Deref for SegmentKey {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }
}

impl std::fmt::Debug for SegmentKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_list().entries(self.iter()).finish()
    }
}

impl PartialEq for SegmentKey {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}

impl Eq for SegmentKey {}

impl PartialOrd for SegmentKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SegmentKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (**self).cmp(&**other)
    }
}

fn column_key(group: u64, token: Token<PropertyKey>) -> SegmentKey {
    let mut key = SegmentKey {
        bytes: [0; SEGMENT_KEY],
        len: 0,
    };
    key.push(SEGMENT_KIND);
    key.push_number(group);
    key.push_number(u64::from(token.id()));
    key
}

/// The key of part `part` of the segment of the column of `group` and `token` that covers the
/// slots from `segment * SEGMENT_SLOTS`.
pub(crate) fn segment_key(
    group: u64,
    token: Token<PropertyKey>,
    segment: usize,
    part: usize,
) -> SegmentKey {
    let mut key = column_key(group, token);
    key.push(segment as u8);
    key.push(part as u8);
    key
}

/// The part a segment key names.
pub(crate) fn part_of_key(key: &[u8]) -> usize {
    key.last().map_or(0, |part| usize::from(*part))
}

/// The prefix of every part of one segment.
pub(crate) fn parts_prefix(group: u64, token: Token<PropertyKey>, segment: usize) -> SegmentKey {
    let key = segment_key(group, token, segment, 0);
    key.cut(key.len() - 1)
}

/// The prefix of every segment of one column.
pub(crate) fn column_prefix(group: u64, token: Token<PropertyKey>) -> SegmentKey {
    column_key(group, token)
}

/// The end, exclusive, of the segment keys of `group`.
pub(crate) fn segments_end(group: u64) -> SegmentKey {
    let mut key = column_key(group, Token::new(0));
    key = key.cut(key.len() - 1);
    key.push(u8::MAX);
    key
}

/// The key of the delta of the node in `slot` of `group`.
pub(crate) fn delta_key(group: u64, slot: usize) -> [u8; DELTA_KEY] {
    let mut key = [0u8; DELTA_KEY];
    key[0] = DELTA_KIND;
    key[1..9].copy_from_slice(&group.to_be_bytes());
    key[9..].copy_from_slice(&(slot as u16).to_be_bytes());
    key
}

/// The prefix of every delta of one group.
pub(crate) fn deltas_prefix(group: u64) -> [u8; 9] {
    let mut prefix = [0u8; 9];
    prefix.copy_from_slice(&delta_key(group, 0)[..9]);
    prefix
}

/// The start, inclusive, and the end, exclusive, of the delta keys of one group.
pub(crate) fn deltas_range(group: u64) -> ([u8; DELTA_KEY], [u8; DELTA_KEY]) {
    (delta_key(group, 0), delta_key(group, SLOTS))
}

/// What a column key names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ColumnKey {
    /// A part of a segment of the column of `group` and `token`.
    Segment {
        group: u64,
        token: Token<PropertyKey>,
        segment: usize,
        part: usize,
    },
    /// The delta of the node in `slot` of `group`.
    Delta { group: u64, slot: usize },
}

fn take_number(key: &[u8], at: &mut usize, most: usize) -> Option<u64> {
    let len = usize::from(*key.get(*at)?);
    let bytes = key.get(*at + 1..*at + 1 + len)?;
    if len > most || bytes.first() == Some(&0) {
        return None;
    }
    *at += 1 + len;
    Some(
        bytes
            .iter()
            .fold(0u64, |number, byte| number << 8 | u64::from(*byte)),
    )
}

fn parse_segment_key(key: &[u8]) -> Option<ColumnKey> {
    let mut at = 1;
    let group = take_number(key, &mut at, 8)?;
    let token = u32::try_from(take_number(key, &mut at, 4)?).ok()?;
    match key.get(at..)? {
        &[segment, part]
            if usize::from(segment) < SEGMENTS && usize::from(part) < SEGMENT_SLOTS =>
        {
            Some(ColumnKey::Segment {
                group,
                token: Token::new(token),
                segment: usize::from(segment),
                part: usize::from(part),
            })
        }
        _ => None,
    }
}

/// The entry a column key names.
pub(crate) fn parse_key(key: &[u8]) -> Result<ColumnKey, GraphError> {
    let unknown = GraphError::Corrupt("a property column key names no entry");
    let group = |key: &[u8]| {
        let mut group = [0u8; 8];
        group.copy_from_slice(&key[1..9]);
        u64::from_be_bytes(group)
    };
    match (key.first(), key.len()) {
        (Some(&SEGMENT_KIND), _) => parse_segment_key(key).ok_or(unknown),
        (Some(&DELTA_KIND), DELTA_KEY) => {
            let slot = usize::from(u16::from_be_bytes([key[9], key[10]]));
            if slot >= SLOTS {
                return Err(unknown);
            }
            Ok(ColumnKey::Delta {
                group: group(key),
                slot,
            })
        }
        _ => Err(unknown),
    }
}

/// How a segment stores its values: one type for every value, or each value encoded with its
/// tag when the values differ in type or are of another type.
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
}

/// The bytes of a stored string. The text of a string segment is checked as UTF-8 only where a
/// string is taken out of it, since a comparison or a search gives the same answer on the bytes
/// of valid text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Text<'a>(&'a [u8]);

impl<'a> Text<'a> {
    pub(crate) fn of(text: &'a str) -> Self {
        Text(text.as_bytes())
    }

    pub(crate) fn bytes(self) -> &'a [u8] {
        self.0
    }

    pub(crate) fn len(self) -> usize {
        self.0.len()
    }

    pub(crate) fn to_str(self) -> Result<&'a str, GraphError> {
        std::str::from_utf8(self.0).map_err(|_| DecodeError::InvalidUtf8.into())
    }
}

/// One stored value, read in place where its segment allows.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Cell<'a> {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(Text<'a>),
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
            PropertyValue::String(value) => Cell::Str(Text::of(value)),
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
                out.extend_from_slice(value.bytes());
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
            Cell::Str(value) => PropertyValue::String(value.to_str()?.to_owned()),
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
            Cell::Str(Text::of(
                std::str::from_utf8(text).map_err(|_| DecodeError::InvalidUtf8)?,
            ))
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

fn part_mask(part: usize, span: usize) -> u64 {
    let run = match span {
        SEGMENT_SLOTS.. => u64::MAX,
        span => (1u64 << span) - 1,
    };
    run.checked_shl(u32::try_from(part * span).unwrap_or(u32::MAX))
        .unwrap_or(0)
}

fn ones(bitmap: u64) -> impl Iterator<Item = usize> {
    let mut rest = bitmap;
    std::iter::from_fn(move || {
        if rest == 0 {
            return None;
        }
        let bit = rest.trailing_zeros() as usize;
        rest &= rest - 1;
        Some(bit)
    })
}

/// A part of a segment: its key and its bytes.
pub(crate) type Part = (SegmentKey, Vec<u8>);

/// The parts that store the values of one segment, each under its key: the positions in the
/// segment that hold a value, ascending, each with its value. Every part covers the same number
/// of positions, its span, except that the last part ends with the segment. The span is the
/// largest under which every part fits `PART_BYTES`, or 1 when none does, and part 0 is stored
/// even when it holds no value, since it tells a reader the span. No part is stored when no
/// position holds a value.
pub(crate) fn encode_segment(
    group: u64,
    token: Token<PropertyKey>,
    segment: usize,
    cells: &[(usize, Cell<'_>)],
) -> Result<Vec<Part>, GraphError> {
    let mut last = None;
    for (at, _) in cells {
        if *at >= SEGMENT_SLOTS {
            return Err(VALUE_OUTSIDE);
        }
        if last.is_some_and(|last| last >= *at) {
            return Err(GraphError::Corrupt(
                "a property column segment takes its values out of order",
            ));
        }
        last = Some(*at);
    }
    if cells.is_empty() {
        return Ok(Vec::new());
    }
    let mut scratch = Vec::new();
    let mut spans = Vec::new();
    for (_, cell) in cells {
        if let Cell::Value(value) = cell {
            let start = scratch.len();
            value.encode_into(&mut scratch);
            spans.push(start..scratch.len());
        }
    }
    let mut spans = spans.into_iter();
    let cells: Vec<(usize, Cell<'_>)> = cells
        .iter()
        .map(|(at, cell)| match cell {
            Cell::Value(_) => spans
                .next()
                .and_then(|span| scratch.get(span))
                .map(|bytes| (*at, Cell::Encoded(bytes)))
                .ok_or(VALUE_OUTSIDE),
            other => Ok((*at, *other)),
        })
        .collect::<Result<_, GraphError>>()?;
    let span = span_of(&cells);
    let mut out = Vec::new();
    if cells.first().is_some_and(|(at, _)| at / span > 0) {
        let key = segment_key(group, token, segment, 0);
        out.push((key, encode_part(&key, span, &[])?));
    }
    for (part, held) in parts_of(&cells, span) {
        let key = segment_key(group, token, segment, part);
        out.push((key, encode_part(&key, span, held)?));
    }
    Ok(out)
}

fn span_of(cells: &[(usize, Cell<'_>)]) -> usize {
    let least: usize = cells.iter().map(|(_, cell)| least_bytes(cell)).sum();
    (1..=SEGMENT_SLOTS)
        .rev()
        .find(|span| {
            values_at(*span) + least.div_ceil(SEGMENT_SLOTS.div_ceil(*span)) <= PART_BYTES
                && parts_of(cells, *span).all(|(_, part)| part_bytes(*span, part) <= PART_BYTES)
        })
        .unwrap_or(1)
}

fn least_bytes(cell: &Cell<'_>) -> usize {
    match cell {
        Cell::Bool(_) => 0,
        Cell::Int(_) | Cell::Float(_) => 8,
        Cell::Str(text) => 2 + text.len(),
        Cell::Encoded(bytes) => 2 + bytes.len(),
        Cell::Value(_) => 2 + tagged_bytes(cell),
    }
}

fn parts_of<'c, 'a>(
    cells: &'c [(usize, Cell<'a>)],
    span: usize,
) -> impl Iterator<Item = (usize, &'c [(usize, Cell<'a>)])> {
    let mut rest = cells;
    std::iter::from_fn(move || {
        let part = rest.first()?.0 / span;
        let split = rest
            .iter()
            .position(|(at, _)| at / span != part)
            .unwrap_or(rest.len());
        let (held, after) = rest.split_at(split);
        rest = after;
        Some((part, held))
    })
}

fn part_kind(cells: &[(usize, Cell<'_>)]) -> Kind {
    cells
        .iter()
        .map(|(_, cell)| cell.kind())
        .reduce(|left, right| if left == right { left } else { Kind::Variant })
        .unwrap_or(Kind::Variant)
}

fn tagged_bytes(cell: &Cell<'_>) -> usize {
    match cell {
        Cell::Bool(_) => 2,
        Cell::Int(_) | Cell::Float(_) => 9,
        Cell::Str(text) => {
            let mut rest = text.len() >> 7;
            let mut varint = 1;
            while rest > 0 {
                varint += 1;
                rest >>= 7;
            }
            1 + varint + text.len()
        }
        Cell::Encoded(bytes) => bytes.len(),
        Cell::Value(value) => {
            let mut bytes = Vec::new();
            value.encode_into(&mut bytes);
            bytes.len()
        }
    }
}

fn bitmap_bytes(span: usize) -> usize {
    span.div_ceil(8).max(1)
}

fn values_at(span: usize) -> usize {
    BITMAP_AT + bitmap_bytes(span)
}

fn offset_bytes(heap: usize) -> usize {
    if heap > usize::from(u16::MAX) { 4 } else { 2 }
}

fn heap_bytes(kind: Kind, cells: &[(usize, Cell<'_>)]) -> usize {
    match kind {
        Kind::Str => cells
            .iter()
            .map(|(_, cell)| match cell {
                Cell::Str(text) => text.len(),
                _ => 0,
            })
            .sum(),
        _ => cells.iter().map(|(_, cell)| tagged_bytes(cell)).sum(),
    }
}

fn part_bytes(span: usize, cells: &[(usize, Cell<'_>)]) -> usize {
    let count = cells.len();
    let kind = part_kind(cells);
    values_at(span)
        + match kind {
            Kind::Bool => count.div_ceil(8),
            Kind::Int | Kind::Float => 8 * count,
            Kind::Str | Kind::Variant => {
                let heap = heap_bytes(kind, cells);
                offset_bytes(heap) * count + heap
            }
        }
}

/// One part of a segment under `key`, in a segment split into parts of `span` positions,
/// holding `cells`.
pub(crate) fn encode_part(
    key: &[u8],
    span: usize,
    cells: &[(usize, Cell<'_>)],
) -> Result<Vec<u8>, GraphError> {
    let kind = part_kind(cells);
    let first = part_of_key(key) * span;
    let bitmap = cells.iter().fold(0u64, |bitmap, (at, _)| {
        bitmap | 1u64 << ((at % SEGMENT_SLOTS).wrapping_sub(first) % SEGMENT_SLOTS)
    });
    let count = cells.len();
    let mut out = Vec::with_capacity(part_bytes(span, cells));
    out.push(LAYOUT_VERSION);
    out.extend_from_slice(&[0; 4]);
    out.push(kind.tag());
    out.push(u8::try_from(span).map_err(|_| UNKNOWN_LAYOUT)?);
    out.extend_from_slice(&bitmap.to_le_bytes()[..bitmap_bytes(span)]);
    match kind {
        Kind::Bool => {
            let start = out.len();
            out.resize(start + count.div_ceil(8), 0);
            for (index, (_, cell)) in cells.iter().enumerate() {
                if matches!(cell, Cell::Bool(true)) {
                    out[start + index / 8] |= 1 << (index % 8);
                }
            }
        }
        Kind::Int | Kind::Float => {
            for (_, cell) in cells {
                match cell {
                    Cell::Int(value) => out.extend_from_slice(&value.to_le_bytes()),
                    Cell::Float(value) => out.extend_from_slice(&value.to_bits().to_le_bytes()),
                    _ => {}
                }
            }
        }
        Kind::Str | Kind::Variant => {
            let mut heap = Vec::with_capacity(heap_bytes(kind, cells));
            let mut ends = Vec::with_capacity(count);
            for (_, cell) in cells {
                match (kind, cell) {
                    (Kind::Str, Cell::Str(text)) => heap.extend_from_slice(text.bytes()),
                    _ => cell.encode_tagged(&mut heap),
                }
                ends.push(heap.len());
            }
            let wide = offset_bytes(heap.len()) == 4;
            if wide {
                out[KIND_AT] |= WIDE_OFFSETS;
            }
            for end in ends {
                match wide {
                    true => out.extend_from_slice(
                        &u32::try_from(end)
                            .map_err(|_| DecodeError::LengthOverflow)?
                            .to_le_bytes(),
                    ),
                    false => out.extend_from_slice(
                        &u16::try_from(end)
                            .map_err(|_| DecodeError::LengthOverflow)?
                            .to_le_bytes(),
                    ),
                }
            }
            out.extend_from_slice(&heap);
        }
    }
    let sum = crc(&[key, &out[KIND_AT..]]);
    out[CHECKSUM_AT..KIND_AT].copy_from_slice(&sum.to_le_bytes());
    Ok(out)
}

#[derive(Clone, Copy, Debug)]
struct Layout {
    kind: Kind,
    span: usize,
    bitmap: u64,
    values: usize,
    offset: usize,
    heap: usize,
}

impl Layout {
    fn of(part: usize, bytes: &[u8]) -> Result<Self, GraphError> {
        if bytes.len() < BITMAP_AT || bytes[0] != LAYOUT_VERSION {
            return Err(UNKNOWN_LAYOUT);
        }
        let flags = bytes[KIND_AT];
        let wide = flags & WIDE_OFFSETS != 0;
        let kind = Kind::from_tag(flags & KIND_BITS).ok_or(UNKNOWN_LAYOUT)?;
        let span = usize::from(bytes[SPAN_AT]);
        if flags & !(KIND_BITS | WIDE_OFFSETS) != 0
            || !(1..=SEGMENT_SLOTS).contains(&span)
            || part * span >= SEGMENT_SLOTS
        {
            return Err(UNKNOWN_LAYOUT);
        }
        let values = values_at(span);
        let mut word = [0u8; 8];
        word[..values - BITMAP_AT].copy_from_slice(
            bytes
                .get(BITMAP_AT..values)
                .ok_or(DecodeError::UnexpectedEnd)?,
        );
        let relative = u64::from_le_bytes(word);
        let bitmap = relative << (part * span);
        if relative & !part_mask(0, span) != 0 || bitmap >> (part * span) != relative {
            return Err(VALUE_OUTSIDE);
        }
        let count = bitmap.count_ones() as usize;
        if count == 0 && (part > 0 || span == SEGMENT_SLOTS) {
            return Err(UNKNOWN_LAYOUT);
        }
        let offset = if wide { 4 } else { 2 };
        let (heap, end) = match kind {
            Kind::Bool | Kind::Int | Kind::Float if wide => return Err(UNKNOWN_LAYOUT),
            Kind::Bool => (values, values + count.div_ceil(8)),
            Kind::Int | Kind::Float => (values, values + 8 * count),
            Kind::Str | Kind::Variant => {
                let heap = values + offset * count;
                if bytes.len() < heap {
                    return Err(DecodeError::UnexpectedEnd.into());
                }
                let last = match count {
                    0 => 0,
                    count => read_offset(bytes, values + offset * (count - 1), offset),
                };
                (heap, heap + last)
            }
        };
        match end.cmp(&bytes.len()) {
            std::cmp::Ordering::Less => Err(DecodeError::TrailingBytes.into()),
            std::cmp::Ordering::Greater => Err(DecodeError::UnexpectedEnd.into()),
            std::cmp::Ordering::Equal => Ok(Self {
                kind,
                span,
                bitmap,
                values,
                offset,
                heap,
            }),
        }
    }
}

fn read_offset(bytes: &[u8], at: usize, size: usize) -> usize {
    match size {
        4 => u32_le(bytes, at) as usize,
        _ => usize::from(u16::from_le_bytes([bytes[at], bytes[at + 1]])),
    }
}

/// A stored segment read in place, checked whole against its checksum.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SegmentRef<'a> {
    bytes: &'a [u8],
    layout: Layout,
}

impl<'a> SegmentRef<'a> {
    /// The part of a segment stored under `key`, checked.
    pub(crate) fn parse(key: &[u8], bytes: &'a [u8]) -> Result<Self, GraphError> {
        if bytes.len() < BITMAP_AT || key.first() != Some(&SEGMENT_KIND) {
            return Err(UNKNOWN_LAYOUT);
        }
        if crc(&[key, &bytes[KIND_AT..]]) != u32_le(bytes, CHECKSUM_AT) {
            return Err(CHECKSUM_MISMATCH);
        }
        Ok(Self {
            bytes,
            layout: Layout::of(part_of_key(key), bytes)?,
        })
    }

    /// The part of its segment that holds position `at`, by the span this part records.
    pub(crate) fn part_of(&self, at: usize) -> usize {
        (at % SEGMENT_SLOTS) / self.layout.span
    }

    fn end_of(&self, index: usize) -> usize {
        let layout = self.layout;
        read_offset(
            self.bytes,
            layout.values + layout.offset * index,
            layout.offset,
        )
    }

    fn cell(&self, index: usize) -> Result<Cell<'a>, GraphError> {
        let bytes = self.bytes;
        let fixed = |at: usize| {
            let mut value = [0u8; 8];
            value.copy_from_slice(&bytes[at..at + 8]);
            value
        };
        let layout = self.layout;
        let values = layout.values;
        Ok(match layout.kind {
            Kind::Bool => Cell::Bool(bytes[values + index / 8] & (1 << (index % 8)) != 0),
            Kind::Int => Cell::Int(i64::from_le_bytes(fixed(values + 8 * index))),
            Kind::Float => Cell::Float(f64::from_bits(u64::from_le_bytes(fixed(
                values + 8 * index,
            )))),
            Kind::Str | Kind::Variant => {
                let start = match index {
                    0 => 0,
                    index => self.end_of(index - 1),
                };
                let value = bytes
                    .get(layout.heap + start..layout.heap + self.end_of(index))
                    .ok_or(VALUE_OUTSIDE)?;
                if layout.kind == Kind::Str {
                    Cell::Str(Text(value))
                } else {
                    classify(value)?
                }
            }
        })
    }

    /// The value at position `at` of the segment, which this part must cover.
    pub(crate) fn cell_at(&self, at: usize) -> Result<Option<Cell<'a>>, GraphError> {
        let bit = 1u64 << (at % SEGMENT_SLOTS);
        if self.layout.bitmap & bit == 0 {
            return Ok(None);
        }
        let index = (self.layout.bitmap & (bit - 1)).count_ones() as usize;
        self.cell(index).map(Some)
    }

    /// Every position that holds a value, ascending, with its value.
    pub(crate) fn cells(&self) -> impl Iterator<Item = Result<(usize, Cell<'a>), GraphError>> + '_ {
        ones(self.layout.bitmap)
            .enumerate()
            .map(|(index, at)| Ok((at, self.cell(index)?)))
    }
}

/// A stored segment copied out of storage and checked, for a reader that reads many values of
/// its column.
#[derive(Debug)]
pub(crate) struct Segment {
    part: usize,
    bytes: Box<[u8]>,
    layout: Layout,
}

impl Segment {
    pub(crate) fn load(key: &[u8], bytes: &[u8]) -> Result<Self, GraphError> {
        let layout = SegmentRef::parse(key, bytes)?.layout;
        Ok(Self {
            part: part_of_key(key),
            bytes: bytes.into(),
            layout,
        })
    }

    fn view(&self) -> SegmentRef<'_> {
        SegmentRef {
            bytes: &self.bytes,
            layout: self.layout,
        }
    }
}

/// The stored segments of one column of a group, loaded for a reader: the parts of each
/// segment, ascending.
#[derive(Debug)]
pub(crate) struct Column {
    segments: Vec<Vec<Segment>>,
}

impl Column {
    /// A column with no stored segment.
    pub(crate) fn new() -> Self {
        Self {
            segments: (0..SEGMENTS).map(|_| Vec::new()).collect(),
        }
    }

    /// Keeps a part of the segment `segment`, loaded in ascending key order.
    pub(crate) fn insert(&mut self, segment: usize, loaded: Segment) {
        if let Some(held) = self.segments.get_mut(segment) {
            held.push(loaded);
        }
    }

    /// The value in `slot` of the group.
    pub(crate) fn cell_at(&self, slot: usize) -> Result<Option<Cell<'_>>, GraphError> {
        let Some(parts) = self.segments.get(slot / SEGMENT_SLOTS) else {
            return Ok(None);
        };
        let Some(first) = parts.first() else {
            return Ok(None);
        };
        let part = first.view().part_of(slot);
        match parts.binary_search_by_key(&part, |held| held.part) {
            Ok(index) => match parts.get(index) {
                Some(held) => held.view().cell_at(slot % SEGMENT_SLOTS),
                None => Ok(None),
            },
            Err(_) => Ok(None),
        }
    }
}

/// What a scan knows of a value without decoding it.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Quick {
    Absent,
    Int(i64),
    /// A value to read in full.
    Unknown,
}

#[derive(Clone, Copy, Debug)]
enum Placed {
    Absent,
    Int(i64),
    Float(f64),
    Heap(u32),
}

/// A [`Column`] with where each slot's value lies, for a scan that reads the value of many slots.
/// An integer or a float is held decoded.
#[derive(Debug)]
pub(crate) struct IndexedColumn {
    column: Column,
    at: Vec<Placed>,
}

impl IndexedColumn {
    pub(crate) fn new(column: Column) -> Self {
        let mut at = vec![Placed::Absent; SLOTS];
        for (segment, parts) in column.segments.iter().enumerate() {
            let Some(first) = parts.first() else {
                continue;
            };
            let span = first.layout.span;
            for (held, part) in parts.iter().enumerate() {
                let layout = part.layout;
                let words = match layout.kind {
                    Kind::Int | Kind::Float => part.bytes.get(layout.values..).unwrap_or_default(),
                    Kind::Bool | Kind::Str | Kind::Variant => &[],
                };
                let words = words.as_chunks::<8>().0;
                for (index, position) in ones(layout.bitmap).enumerate() {
                    if position / span != part.part {
                        continue;
                    }
                    let placed = match words.get(index) {
                        Some(word) if layout.kind == Kind::Int => {
                            Placed::Int(i64::from_le_bytes(*word))
                        }
                        Some(word) => Placed::Float(f64::from_le_bytes(*word)),
                        None => {
                            Placed::Heap((segment as u32) << 16 | (held as u32) << 8 | index as u32)
                        }
                    };
                    if let Some(slot) = at.get_mut(segment * SEGMENT_SLOTS + position) {
                        *slot = placed;
                    }
                }
            }
        }
        Self { column, at }
    }

    /// The value in `slot` of the group where it is absent or an integer.
    pub(crate) fn quick(&self, slot: usize) -> Quick {
        match self.at.get(slot) {
            None | Some(Placed::Absent) => Quick::Absent,
            Some(Placed::Int(value)) => Quick::Int(*value),
            Some(Placed::Float(_) | Placed::Heap(_)) => Quick::Unknown,
        }
    }

    /// The value in `slot` of the group, as [`Column::cell_at`] reads it.
    pub(crate) fn cell_at(&self, slot: usize) -> Result<Option<Cell<'_>>, GraphError> {
        let at = match self.at.get(slot) {
            None | Some(Placed::Absent) => return Ok(None),
            Some(Placed::Int(value)) => return Ok(Some(Cell::Int(*value))),
            Some(Placed::Float(value)) => return Ok(Some(Cell::Float(*value))),
            Some(Placed::Heap(at)) => *at as usize,
        };
        match self
            .column
            .segments
            .get(at >> 16)
            .and_then(|parts| parts.get(at >> 8 & 0xff))
        {
            Some(part) => part.view().cell(at & 0xff).map(Some),
            None => Ok(None),
        }
    }
}

/// The delta stored for one node under `key`: its properties that differ from the segments of
/// its group, each with its value now or none when it holds none. The properties ascend by key.
pub(crate) fn encode_delta<'a>(
    key: &[u8; DELTA_KEY],
    entries: impl IntoIterator<Item = (Token<PropertyKey>, Option<Cell<'a>>)>,
) -> Vec<u8> {
    let mut bytes = vec![LAYOUT_VERSION];
    for (token, value) in entries {
        bytes.extend_from_slice(&token.id().to_be_bytes());
        match value {
            Some(value) => {
                bytes.push(PRESENT);
                value.encode_tagged(&mut bytes);
            }
            None => bytes.push(ABSENT),
        }
    }
    let sum = crc(&[key, &bytes]);
    bytes.extend_from_slice(&sum.to_le_bytes());
    bytes
}

/// The delta stored for one node under `key` that holds the properties of `created`, a node as
/// a transaction created it.
pub(crate) fn encode_created_delta(key: &[u8; DELTA_KEY], created: DeltaRef<'_>) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(created.body.len() + 5);
    bytes.push(LAYOUT_VERSION);
    bytes.extend_from_slice(created.body);
    let sum = crc(&[key, &bytes]);
    bytes.extend_from_slice(&sum.to_le_bytes());
    bytes
}

fn next_entry<'a>(
    reader: &mut Reader<'a>,
    body: &'a [u8],
) -> Result<(Token<PropertyKey>, Option<&'a [u8]>), GraphError> {
    let token = Token::new(reader.u32()?);
    match reader.u8()? {
        ABSENT => Ok((token, None)),
        PRESENT => {
            let start = body.len() - reader.remaining();
            reader.skip_value()?;
            let end = body.len() - reader.remaining();
            Ok((token, Some(&body[start..end])))
        }
        _ => Err(GraphError::Corrupt(
            "a property column delta holds no slot state",
        )),
    }
}

/// A stored delta read in place, checked against its checksum.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DeltaRef<'a> {
    body: &'a [u8],
}

impl<'a> DeltaRef<'a> {
    /// The delta stored under `key`, checked.
    pub(crate) fn parse(key: &[u8], bytes: &'a [u8]) -> Result<Self, GraphError> {
        let end = bytes
            .len()
            .checked_sub(4)
            .filter(|end| *end > 1)
            .ok_or(DecodeError::UnexpectedEnd)?;
        if crc(&[key, &bytes[..end]]) != u32_le(bytes, end) {
            return Err(CHECKSUM_MISMATCH);
        }
        if bytes[0] != LAYOUT_VERSION {
            return Err(GraphError::Corrupt(
                "a property column delta has an unknown layout",
            ));
        }
        let delta = Self {
            body: &bytes[1..end],
        };
        let mut last: Option<Token<PropertyKey>> = None;
        for entry in delta.raw() {
            let (token, _) = entry?;
            if last.is_some_and(|last| last >= token) {
                return Err(GraphError::Corrupt(
                    "a property column delta lists its properties out of order",
                ));
            }
            last = Some(token);
        }
        Ok(delta)
    }

    fn raw(
        &self,
    ) -> impl Iterator<Item = Result<(Token<PropertyKey>, Option<&'a [u8]>), GraphError>> {
        let body = self.body;
        let mut reader = Reader::new(body);
        let mut failed = false;
        std::iter::from_fn(move || {
            if failed || reader.remaining() == 0 {
                return None;
            }
            let entry = next_entry(&mut reader, body);
            failed = entry.is_err();
            Some(entry)
        })
    }

    /// Every property the delta holds, ascending by key, with its value now.
    pub(crate) fn entries(
        &self,
    ) -> impl Iterator<Item = Result<(Token<PropertyKey>, Option<Cell<'a>>), GraphError>> {
        self.raw().map(|entry| {
            let (token, value) = entry?;
            Ok((token, value.map(classify).transpose()?))
        })
    }

    /// The value of the property `token` now: `None` when the delta leaves it to the segments,
    /// `Some(None)` when the node holds none.
    pub(crate) fn get(
        &self,
        token: Token<PropertyKey>,
    ) -> Result<Option<Option<Cell<'a>>>, GraphError> {
        for entry in self.raw() {
            let (held, value) = entry?;
            if held == token {
                return Ok(Some(value.map(classify).transpose()?));
            }
            if held > token {
                break;
            }
        }
        Ok(None)
    }

    /// Whether the delta holds no property.
    pub(crate) fn is_empty(&self) -> bool {
        self.body.is_empty()
    }

    /// The property keys the delta holds, ascending.
    pub(crate) fn tokens(&self) -> impl Iterator<Item = Result<Token<PropertyKey>, GraphError>> {
        self.raw().map(|entry| entry.map(|(token, _)| token))
    }
}

/// A stored delta copied out of storage and checked, with where each of its entries starts, so
/// one property is found without reading the entries before it.
#[derive(Debug)]
pub(crate) struct Delta {
    bytes: Box<[u8]>,
    starts: Box<[(Token<PropertyKey>, usize)]>,
}

impl Delta {
    pub(crate) fn load(key: &[u8], bytes: &[u8]) -> Result<Self, GraphError> {
        let body = DeltaRef::parse(key, bytes)?.body;
        let mut reader = Reader::new(body);
        let mut starts = Vec::new();
        while reader.remaining() > 0 {
            let start = body.len() - reader.remaining();
            let (token, _) = next_entry(&mut reader, body)?;
            starts.push((token, start));
        }
        Ok(Self {
            bytes: bytes.into(),
            starts: starts.into(),
        })
    }

    pub(crate) fn view(&self) -> DeltaRef<'_> {
        DeltaRef {
            body: &self.bytes[1..self.bytes.len() - 4],
        }
    }

    /// The value of the property `token` now, as [`DeltaRef::get`] gives it.
    pub(crate) fn get(
        &self,
        token: Token<PropertyKey>,
    ) -> Result<Option<Option<Cell<'_>>>, GraphError> {
        let Ok(index) = self.starts.binary_search_by_key(&token, |(held, _)| *held) else {
            return Ok(None);
        };
        let body = self.view().body;
        let rest = body
            .get(self.starts[index].1..)
            .ok_or(DecodeError::UnexpectedEnd)?;
        let (_, value) = next_entry(&mut Reader::new(rest), rest)?;
        Ok(Some(value.map(classify).transpose()?))
    }
}

/// The values a transaction gave one node, by key, each with its value or none for no value.
pub(crate) type SlotValues = BTreeMap<Token<PropertyKey>, Option<PropertyValue>>;

/// Property values by key, ascending by key, each with its value or none for no value.
pub(crate) type Changes = Vec<(Token<PropertyKey>, Option<PropertyValue>)>;

/// Sets `token` to `value` in `changes`.
pub(crate) fn put_change(
    changes: &mut Changes,
    token: Token<PropertyKey>,
    value: Option<PropertyValue>,
) {
    match changes.binary_search_by_key(&token, |(held, _)| *held) {
        Ok(index) => {
            if let Some(held) = changes.get_mut(index) {
                held.1 = value;
            }
        }
        Err(index) => changes.insert(index, (token, value)),
    }
}

/// The values a write transaction set on the node in one slot.
#[derive(Clone, Debug, Default)]
pub(crate) struct SlotEdit {
    /// Whether the transaction created the node, so the slot stores no value.
    pub(crate) fresh: bool,
    created: Option<(usize, usize)>,
    /// The properties the transaction changed, with their values now, over those it created the
    /// node with.
    pub(crate) values: Changes,
}

impl SlotEdit {
    /// The properties the node holds as the transaction left it, ascending by key, where they
    /// differ from the stored segments.
    pub(crate) fn into_changes(self, heap: &[u8]) -> Result<Changes, GraphError> {
        let created = self.created(heap);
        let mut changes = self.values;
        if let Some(created) = created {
            for entry in created.entries() {
                let (token, cell) = entry?;
                if let Err(at) = changes.binary_search_by_key(&token, |(held, _)| *held) {
                    changes.insert(at, (token, cell.map(|cell| cell.to_value()).transpose()?));
                }
            }
        }
        Ok(changes)
    }

    /// The properties the transaction created the node with, ascending by key, in the heap of
    /// its group.
    pub(crate) fn created<'h>(&self, heap: &'h [u8]) -> Option<DeltaRef<'h>> {
        let (start, end) = self.created?;
        Some(DeltaRef {
            body: heap.get(start..end)?,
        })
    }
}

/// The property columns of one row group in a write transaction: how many deltas the group
/// stores and the slots this transaction changed, ascending. A slot the transaction did not
/// change reads as stored. The properties of the nodes the transaction created are kept encoded
/// in one heap, so that a large transaction holds few allocations.
#[derive(Clone, Debug, Default)]
pub(crate) struct ColumnsEdit {
    deltas: usize,
    slots: Vec<(usize, SlotEdit)>,
    heap: Vec<u8>,
}

impl ColumnsEdit {
    /// The group as stored, with `deltas` deltas.
    pub(crate) fn load(deltas: usize) -> Self {
        Self {
            deltas,
            slots: Vec::new(),
            heap: Vec::new(),
        }
    }

    fn find(&self, slot: usize) -> Option<&SlotEdit> {
        let index = self
            .slots
            .binary_search_by_key(&slot, |(held, _)| *held)
            .ok()?;
        self.slots.get(index).map(|(_, edit)| edit)
    }

    fn index(&mut self, slot: usize) -> usize {
        match self.slots.last() {
            Some((last, _)) if *last == slot => self.slots.len() - 1,
            Some((last, _)) if *last > slot => {
                match self.slots.binary_search_by_key(&slot, |(held, _)| *held) {
                    Ok(index) => index,
                    Err(index) => {
                        self.slots.insert(index, (slot, SlotEdit::default()));
                        index
                    }
                }
            }
            _ => {
                self.slots.push((slot, SlotEdit::default()));
                self.slots.len() - 1
            }
        }
    }

    /// The value of the property `token` of the node in `slot` as the transaction sees it, when
    /// the transaction changed it or the slot stores no value: `Some(None)` for no value, `None`
    /// to read the stored columns.
    pub(crate) fn current(
        &self,
        slot: usize,
        token: Token<PropertyKey>,
    ) -> Result<Option<Option<PropertyValue>>, GraphError> {
        self.current_cell(slot, token)?
            .map(|cell| cell.map(Cell::to_value).transpose())
            .transpose()
    }

    /// [`current`](Self::current), read in place.
    pub(crate) fn current_cell(
        &self,
        slot: usize,
        token: Token<PropertyKey>,
    ) -> Result<Option<Option<Cell<'_>>>, GraphError> {
        let Some(edit) = self.find(slot) else {
            return Ok(None);
        };
        if let Ok(index) = edit.values.binary_search_by_key(&token, |(held, _)| *held) {
            return Ok(edit
                .values
                .get(index)
                .map(|(_, value)| value.as_ref().map(Cell::of)));
        }
        if let Some(created) = edit.created(&self.heap)
            && let Some(cell) = created.get(token)?
        {
            return Ok(Some(cell));
        }
        Ok(edit.fresh.then_some(None))
    }

    /// Every value the transaction gave the node in `slot`, with `None` for a value it removed,
    /// and whether the transaction created the node, so that no stored value applies to it.
    pub(crate) fn current_all(&self, slot: usize) -> Result<(SlotValues, bool), GraphError> {
        let Some(edit) = self.find(slot) else {
            return Ok((BTreeMap::new(), false));
        };
        let mut values = BTreeMap::new();
        if let Some(created) = edit.created(&self.heap) {
            for entry in created.entries() {
                let (token, cell) = entry?;
                values.insert(token, cell.map(|cell| cell.to_value()).transpose()?);
            }
        }
        for (token, value) in &edit.values {
            values.insert(*token, value.clone());
        }
        Ok((values, edit.fresh))
    }

    /// Marks the node in `slot` as created by the transaction, holding `values`, ascending by
    /// key, and nothing else.
    pub(crate) fn create(
        &mut self,
        slot: usize,
        values: impl IntoIterator<Item = (Token<PropertyKey>, PropertyValue)>,
    ) -> Result<(), GraphError> {
        let index = self.index(slot);
        let heap = &mut self.heap;
        let Some((_, edit)) = self.slots.get_mut(index) else {
            return Err(GraphError::Corrupt("a property column edit lost a slot"));
        };
        if edit.values.is_empty() && edit.created.is_none() {
            if heap.capacity() == 0 {
                heap.reserve(CREATED_BYTES);
            }
            let start = heap.len();
            for (token, value) in values {
                heap.extend_from_slice(&token.id().to_be_bytes());
                heap.push(PRESENT);
                Cell::of(&value).encode_tagged(heap);
            }
            edit.fresh = true;
            edit.created = Some((start, heap.len()));
            return Ok(());
        }
        let mut changes = std::mem::take(edit).into_changes(heap)?;
        for (token, value) in values {
            put_change(&mut changes, token, Some(value));
        }
        edit.fresh = true;
        edit.values = changes;
        Ok(())
    }

    /// Sets the value of the property `token` of the node in `slot`, or removes it with `None`.
    pub(crate) fn set(
        &mut self,
        slot: usize,
        token: Token<PropertyKey>,
        value: Option<PropertyValue>,
    ) {
        let index = self.index(slot);
        if let Some((_, edit)) = self.slots.get_mut(index) {
            put_change(&mut edit.values, token, value);
        }
    }

    /// The property keys the transaction changed or created the node in `slot` with.
    pub(crate) fn tokens(&self, slot: usize) -> Result<Vec<Token<PropertyKey>>, GraphError> {
        let Some(edit) = self.find(slot) else {
            return Ok(Vec::new());
        };
        let mut tokens: Vec<Token<PropertyKey>> =
            edit.values.iter().map(|(token, _)| *token).collect();
        if let Some(created) = edit.created(&self.heap) {
            for token in created.tokens() {
                tokens.push(token?);
            }
            tokens.sort_unstable();
            tokens.dedup();
        }
        Ok(tokens)
    }

    /// How many deltas the group stores, the slots the transaction changed, ascending, with
    /// their changes, and the heap that holds the properties of the nodes it created.
    pub(crate) fn into_slots(self) -> (usize, Vec<(usize, SlotEdit)>, Vec<u8>) {
        (self.deltas, self.slots, self.heap)
    }
}

/// The stored values of a segment merged with changes, ascending by position, as a segment takes
/// them.
pub(crate) fn merged<'a>(
    stored: impl Iterator<Item = Result<(usize, Cell<'a>), GraphError>>,
    changes: &[(usize, Option<Cell<'a>>)],
) -> Result<Vec<(usize, Cell<'a>)>, GraphError> {
    let mut cells = Vec::with_capacity(SEGMENT_SLOTS);
    let mut changes = changes.iter().peekable();
    for entry in stored {
        let (at, cell) = entry?;
        while let Some((changed, value)) = changes.next_if(|change| change.0 < at) {
            if let Some(value) = value {
                cells.push((*changed, *value));
            }
        }
        match changes.next_if(|change| change.0 == at) {
            Some((_, Some(value))) => cells.push((at, *value)),
            Some((_, None)) => {}
            None => cells.push((at, cell)),
        }
    }
    for (changed, value) in changes {
        if let Some(value) = value {
            cells.push((*changed, *value));
        }
    }
    Ok(cells)
}

/// A stored column or set of deltas a reader read in place so far, or loaded whole.
#[derive(Debug)]
pub(crate) enum Cached<P, L> {
    /// Read in place: a column after fewer than `PROBES` reads, a group's deltas after one.
    Probed(P),
    /// Loaded whole.
    Loaded(L),
}

/// The deltas of one group a reader loaded, ascending by slot.
pub(crate) type Deltas = Vec<(usize, Delta)>;

/// The delta of one slot a reader read in place, or none, or every delta of the group, loaded.
pub(crate) type CachedDeltas = Cached<(usize, Option<Delta>), Deltas>;

/// How many values of one column of a group a query reads in place before it loads the column
/// whole. Loading a full column costs about as much as this many reads in place, so a query that
/// reads a few nodes of a group, as a traversal does, never loads it.
pub(crate) const PROBES: usize = 16;

/// The stored columns and deltas one query read so far. A column read fewer than `PROBES` times
/// and a group's deltas read once are read in place, so a point lookup copies nothing; read more,
/// they are loaded and checked whole, so a scan reads each value with one storage access per
/// segment.
#[derive(Debug, Default)]
pub(crate) struct ColumnReader {
    pub(crate) columns: Vec<(ColumnId, Cached<usize, Column>)>,
    pub(crate) deltas: Vec<(u64, CachedDeltas)>,
    pub(crate) keys: Vec<(u64, Vec<Token<PropertyKey>>)>,
}

impl ColumnReader {
    /// The property keys with a stored segment in `group`, if read before.
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

/// The delta of `slot` among a group's loaded deltas.
pub(crate) fn find_delta(deltas: &Deltas, slot: usize) -> Option<&Delta> {
    deltas
        .binary_search_by_key(&slot, |(held, _)| *held)
        .ok()
        .map(|index| &deltas[index].1)
}

#[cfg(test)]
#[path = "columns.test.rs"]
mod tests;
