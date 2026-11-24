use std::collections::BTreeMap;
use std::collections::BTreeSet;

use crate::value::DecodeError;

use super::GraphError;
use super::ids::NodeId;
use super::ids::RelationshipId;
use super::meta;

const LAYOUT_VERSION: u8 = 1;
/// How many consecutive slots of a row group one adjacency segment covers.
pub(crate) const SEGMENT_SLOTS: usize = 16;
const SEGMENT_SHIFT: u32 = 4;
const DELTA_KIND: u8 = 0;
const PART_KIND: u8 = 1;
const SIDE_AT: usize = 0;
const GROUP_AT: usize = 1;
const SEGMENT_AT: usize = 9;
const TYPE_AT: usize = 10;
const KIND_AT: usize = 14;
/// The length of the prefix every entry of one segment of one relationship type shares.
pub(crate) const TYPED_PREFIX: usize = 14;
const DELTA_KEY: usize = 16;
const PART_KEY: usize = 17;
const CHECKSUM_AT: usize = 1;
const FLAGS_AT: usize = 5;
const OFFSETS_AT: usize = 6;
const WIDE_OFFSETS: u8 = 1;
/// The size a part stays within unless one slot alone holds more, so the storage engine packs
/// several parts with their keys into one page.
pub(crate) const PART_BYTES: usize = super::columns::PART_BYTES;
/// The most deltas one segment of one relationship type keeps beside its parts.
pub(crate) const MAX_DELTAS: usize = 4;
/// The largest delta a commit writes. A commit whose delta would be larger rewrites the segment.
pub(crate) const DELTA_BYTES: usize = PART_BYTES / 2;

const CHECKSUM_MISMATCH: GraphError =
    GraphError::Corrupt("an adjacency entry does not match its checksum");
const UNKNOWN_LAYOUT: GraphError = GraphError::Corrupt("an adjacency entry has an unknown layout");
const NOT_CANONICAL: GraphError = GraphError::Corrupt("an adjacency entry is not canonical");
const SPANS_DISAGREE: GraphError =
    GraphError::Corrupt("the parts of an adjacency segment disagree on their span");
const DELTA_WITHOUT_PARTS: GraphError = GraphError::Corrupt("an adjacency delta has no segment");
const DELTA_DISAGREES: GraphError =
    GraphError::Corrupt("an adjacency delta does not amend its segment");
const BAD_KEY: GraphError = GraphError::Corrupt("an adjacency key names no entry");

/// A stored adjacency entry: its key and its bytes.
pub(crate) type Stored = (Vec<u8>, Vec<u8>);

/// The slots of a segment that hold relationships, ascending, each with its relationships.
#[cfg(test)]
pub(crate) type Slots = Vec<(usize, Vec<Edge>)>;

type Held = (Box<[u8]>, Box<[u8]>);

/// Which end of its relationships a node's adjacency lists: the relationships it is the source
/// of, or the ones it is the target of.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum Side {
    Outgoing,
    Incoming,
}

impl Side {
    fn byte(self) -> u8 {
        match self {
            Side::Outgoing => 0,
            Side::Incoming => 1,
        }
    }

    fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Side::Outgoing),
            1 => Some(Side::Incoming),
            _ => None,
        }
    }
}

/// One segment of one relationship type on one side of a row group: the unit adjacency is
/// stored, edited, and checked in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct SegmentId {
    pub(crate) side: Side,
    pub(crate) group: u64,
    pub(crate) segment: u8,
    pub(crate) rel_type: u32,
}

/// The segment that lists `node`'s relationships of `rel_type` on `side`, and the node's
/// position in it.
pub(crate) fn locate(side: Side, node: NodeId, rel_type: u32) -> (SegmentId, usize) {
    let (group, slot) = meta::locate(node);
    let segment = SegmentId {
        side,
        group,
        segment: (slot >> SEGMENT_SHIFT) as u8,
        rel_type,
    };
    (segment, slot % SEGMENT_SLOTS)
}

impl SegmentId {
    /// The node at `position` of the segment.
    #[cfg(test)]
    pub(crate) fn node_at(&self, position: usize) -> NodeId {
        meta::node_at(
            self.group,
            (usize::from(self.segment) << SEGMENT_SHIFT) | position,
        )
    }

    /// The prefix every entry of the segment shares.
    pub(crate) fn prefix(&self) -> [u8; TYPED_PREFIX] {
        let mut key = [0u8; TYPED_PREFIX];
        key[..TYPE_AT].copy_from_slice(&segment_prefix(self.side, self.group, self.segment));
        key[TYPE_AT..].copy_from_slice(&self.rel_type.to_be_bytes());
        key
    }

    /// The bounds of the keys of every entry of the segment.
    pub(crate) fn range(&self) -> ([u8; TYPED_PREFIX + 1], [u8; TYPED_PREFIX + 1]) {
        let mut start = [0u8; TYPED_PREFIX + 1];
        start[..TYPED_PREFIX].copy_from_slice(&self.prefix());
        let mut end = start;
        start[KIND_AT] = DELTA_KIND;
        end[KIND_AT] = PART_KIND + 1;
        (start, end)
    }

    pub(crate) fn delta_key(&self, position: usize) -> [u8; DELTA_KEY] {
        let mut key = [0u8; DELTA_KEY];
        key[..TYPED_PREFIX].copy_from_slice(&self.prefix());
        key[KIND_AT] = DELTA_KIND;
        key[KIND_AT + 1] = position as u8;
        key
    }

    pub(crate) fn part_key(&self, span: usize, index: usize) -> [u8; PART_KEY] {
        let mut key = [0u8; PART_KEY];
        key[..TYPED_PREFIX].copy_from_slice(&self.prefix());
        key[KIND_AT] = PART_KIND;
        key[KIND_AT + 1] = span as u8;
        key[KIND_AT + 2] = index as u8;
        key
    }
}

/// The prefix of every entry of one segment on one side of a group, of every relationship type.
pub(crate) fn segment_prefix(side: Side, group: u64, segment: u8) -> [u8; TYPE_AT] {
    let mut key = [0u8; TYPE_AT];
    key[SIDE_AT] = side.byte();
    key[GROUP_AT..SEGMENT_AT].copy_from_slice(&group.to_be_bytes());
    key[SEGMENT_AT] = segment;
    key
}

/// What an adjacency key names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Entry {
    /// The delta of the slot at this position.
    Delta(usize),
    /// Part `index` of a segment split into parts of `span` positions.
    Part { span: usize, index: usize },
}

impl Entry {
    /// Whether the entry holds the slot at `position`.
    pub(crate) fn covers(self, position: usize) -> bool {
        match self {
            Entry::Delta(at) => at == position,
            Entry::Part { span, index } => position / span == index,
        }
    }
}

/// The segment and entry an adjacency key names.
pub(crate) fn parse_key(key: &[u8]) -> Result<(SegmentId, Entry), GraphError> {
    if key.len() != DELTA_KEY && key.len() != PART_KEY {
        return Err(BAD_KEY);
    }
    let Some(side) = Side::from_byte(key[SIDE_AT]) else {
        return Err(BAD_KEY);
    };
    let mut group = [0u8; 8];
    group.copy_from_slice(&key[GROUP_AT..SEGMENT_AT]);
    let segment = key[SEGMENT_AT];
    let rel_type = u32::from_be_bytes([
        key[TYPE_AT],
        key[TYPE_AT + 1],
        key[TYPE_AT + 2],
        key[TYPE_AT + 3],
    ]);
    let id = SegmentId {
        side,
        group: u64::from_be_bytes(group),
        segment,
        rel_type,
    };
    let entry = match (key[KIND_AT], key.len()) {
        (DELTA_KIND, DELTA_KEY) => {
            let position = usize::from(key[KIND_AT + 1]);
            if position >= SEGMENT_SLOTS {
                return Err(BAD_KEY);
            }
            Entry::Delta(position)
        }
        (PART_KIND, PART_KEY) => {
            let span = usize::from(key[KIND_AT + 1]);
            let index = usize::from(key[KIND_AT + 2]);
            if span == 0 || span > SEGMENT_SLOTS || index >= SEGMENT_SLOTS.div_ceil(span) {
                return Err(BAD_KEY);
            }
            Entry::Part { span, index }
        }
        _ => return Err(BAD_KEY),
    };
    Ok((id, entry))
}

/// One relationship in a node's adjacency: its ID and the node at its other end.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct Edge {
    pub(crate) relationship: RelationshipId,
    pub(crate) other: NodeId,
}

fn put_varint(out: &mut Vec<u8>, value: u64) {
    let mut rest = value;
    while rest >= 0x80 {
        out.push((rest & 0x7f) as u8 | 0x80);
        rest >>= 7;
    }
    out.push(rest as u8);
}

fn varint_len(value: u64) -> usize {
    (64 - value.leading_zeros() as usize).div_ceil(7).max(1)
}

#[derive(Debug)]
struct Varints<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Varints<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    fn is_done(&self) -> bool {
        self.at == self.bytes.len()
    }

    fn next(&mut self) -> Result<u64, GraphError> {
        if let Some(byte) = self.bytes.get(self.at).copied()
            && byte < 0x80
        {
            self.at += 1;
            return Ok(u64::from(byte));
        }
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let Some(byte) = self.bytes.get(self.at).copied() else {
                return Err(DecodeError::UnexpectedEnd.into());
            };
            self.at += 1;
            let bits = u64::from(byte & 0x7f);
            if bits << shift >> shift != bits {
                return Err(DecodeError::LengthOverflow.into());
            }
            value |= bits << shift;
            if byte & 0x80 == 0 {
                if byte == 0 && shift > 0 {
                    return Err(NOT_CANONICAL);
                }
                return Ok(value);
            }
        }
        Err(DecodeError::LengthOverflow.into())
    }

    fn count(&mut self) -> Result<usize, GraphError> {
        let count = usize::try_from(self.next()?).map_err(|_| DecodeError::LengthOverflow)?;
        if count > self.bytes.len() - self.at {
            return Err(DecodeError::UnexpectedEnd.into());
        }
        Ok(count)
    }

    fn finish(&self) -> Result<(), GraphError> {
        if self.is_done() {
            Ok(())
        } else {
            Err(DecodeError::TrailingBytes.into())
        }
    }
}

#[derive(Debug, Default)]
struct Ascending {
    last: Option<u64>,
}

impl Ascending {
    fn put(&mut self, out: &mut Vec<u8>, relationship: RelationshipId) {
        let gap = match self.last {
            Some(last) => relationship.0 - last,
            None => relationship.0,
        };
        self.last = Some(relationship.0);
        put_varint(out, gap);
    }

    fn len(&mut self, relationship: RelationshipId) -> usize {
        let gap = match self.last {
            Some(last) => relationship.0 - last,
            None => relationship.0,
        };
        self.last = Some(relationship.0);
        varint_len(gap)
    }

    fn next(&mut self, varints: &mut Varints<'_>) -> Result<RelationshipId, GraphError> {
        let gap = varints.next()?;
        let relationship = match self.last {
            None => gap,
            Some(_) if gap == 0 => return Err(NOT_CANONICAL),
            Some(last) => last.checked_add(gap).ok_or(DecodeError::LengthOverflow)?,
        };
        self.last = Some(relationship);
        Ok(RelationshipId(relationship))
    }
}

fn edges_len(edges: &[Edge]) -> usize {
    let mut ascending = Ascending::default();
    edges
        .iter()
        .map(|edge| ascending.len(edge.relationship) + varint_len(edge.other.0))
        .sum()
}

fn put_edges(out: &mut Vec<u8>, edges: &[Edge]) {
    let mut ascending = Ascending::default();
    for edge in edges {
        ascending.put(out, edge.relationship);
        put_varint(out, edge.other.0);
    }
}

fn visit_edges(
    bytes: &[u8],
    mut visit: impl FnMut(Edge) -> Result<(), GraphError>,
) -> Result<(), GraphError> {
    let mut varints = Varints::new(bytes);
    let mut ascending = Ascending::default();
    while !varints.is_done() {
        let relationship = ascending.next(&mut varints)?;
        let other = NodeId(varints.next()?);
        visit(Edge {
            relationship,
            other,
        })?;
    }
    Ok(())
}

fn crc(key: &[u8], body: &[u8]) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(key);
    hasher.update(body);
    hasher.finalize()
}

fn seal(key: &[u8], bytes: &mut [u8]) {
    let sum = crc(key, &bytes[FLAGS_AT..]);
    bytes[CHECKSUM_AT..FLAGS_AT].copy_from_slice(&sum.to_le_bytes());
}

fn checked(key: &[u8], bytes: &[u8]) -> Result<(), GraphError> {
    if bytes.len() < FLAGS_AT || bytes[0] != LAYOUT_VERSION {
        return Err(UNKNOWN_LAYOUT);
    }
    let stored = u32::from_le_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]);
    if crc(key, &bytes[FLAGS_AT..]) != stored {
        return Err(CHECKSUM_MISMATCH);
    }
    Ok(())
}

fn run_of(span: usize, index: usize) -> usize {
    span.min(SEGMENT_SLOTS - index * span)
}

fn offset_width(data: usize) -> usize {
    if data > usize::from(u16::MAX) { 4 } else { 2 }
}

fn part_len(run: usize, data: usize) -> usize {
    OFFSETS_AT + offset_width(data) * (run - 1) + data
}

/// The stored parts of one segment: each part's key and bytes. `slots` holds the positions that
/// have relationships, ascending, each with its relationships, ascending by ID. Every part
/// covers the same number of positions, its span, except that the last ends with the segment.
/// The span is the largest under which every part fits `PART_BYTES`, or 1 when none does. A
/// part that would hold no relationship is not stored.
pub(crate) fn encode_segment(
    id: &SegmentId,
    slots: &[(usize, &[Edge])],
) -> Result<Vec<Stored>, GraphError> {
    let encoded = encode_slots(slots)?;
    let borrowed: Vec<(usize, &[u8])> = encoded
        .iter()
        .map(|(position, bytes)| (*position, bytes.as_slice()))
        .collect();
    Ok(lay_out(id, &borrowed, None))
}

fn encode_slots(slots: &[(usize, &[Edge])]) -> Result<Vec<(usize, Vec<u8>)>, GraphError> {
    let mut last = None;
    let mut out = Vec::with_capacity(slots.len());
    for (position, edges) in slots {
        if *position >= SEGMENT_SLOTS || last.is_some_and(|last| last >= *position) {
            return Err(GraphError::Corrupt(
                "an adjacency segment takes its slots out of order",
            ));
        }
        if !edges.is_sorted_by(|left, right| left.relationship < right.relationship) {
            return Err(GraphError::Corrupt(
                "an adjacency slot takes its relationships out of order",
            ));
        }
        last = Some(*position);
        let mut bytes = Vec::with_capacity(edges_len(edges));
        put_edges(&mut bytes, edges);
        out.push((*position, bytes));
    }
    Ok(out)
}

/// The parts of a segment split by `span`, which need not be the span its slots call for.
#[cfg(test)]
pub(crate) fn encode_segment_by(
    id: &SegmentId,
    slots: &[(usize, &[Edge])],
    span: usize,
) -> Vec<Stored> {
    let encoded = encode_slots(slots).unwrap_or_default();
    let borrowed: Vec<(usize, &[u8])> = encoded
        .iter()
        .map(|(position, bytes)| (*position, bytes.as_slice()))
        .collect();
    lay_out(id, &borrowed, Some(span))
}

fn lay_out(id: &SegmentId, slots: &[(usize, &[u8])], span: Option<usize>) -> Vec<Stored> {
    let mut sizes = [0usize; SEGMENT_SLOTS];
    for (position, bytes) in slots {
        sizes[*position] = bytes.len();
    }
    let fits = |span: usize| {
        (0..SEGMENT_SLOTS.div_ceil(span)).all(|index| {
            let start = index * span;
            let run = run_of(span, index);
            let data: usize = sizes[start..start + run].iter().sum();
            data == 0 || part_len(run, data) <= PART_BYTES
        })
    };
    let span = span.unwrap_or_else(|| {
        (2..=SEGMENT_SLOTS)
            .rev()
            .find(|span| fits(*span))
            .unwrap_or(1)
    });
    let mut out = Vec::new();
    let mut rest = slots;
    for index in 0..SEGMENT_SLOTS.div_ceil(span) {
        let start = index * span;
        let run = run_of(span, index);
        let split = rest
            .iter()
            .position(|(position, _)| *position >= start + run)
            .unwrap_or(rest.len());
        let (held, after) = rest.split_at(split);
        rest = after;
        let data: usize = sizes[start..start + run].iter().sum();
        if data == 0 {
            continue;
        }
        let key = id.part_key(span, index);
        let width = offset_width(data);
        let mut bytes = Vec::with_capacity(part_len(run, data));
        bytes.push(LAYOUT_VERSION);
        bytes.extend_from_slice(&[0; 4]);
        bytes.push(if width == 4 { WIDE_OFFSETS } else { 0 });
        let mut end = 0;
        for size in &sizes[start..start + run - 1] {
            end += size;
            bytes.extend_from_slice(&(end as u32).to_le_bytes()[..width]);
        }
        for (_, slot) in held {
            bytes.extend_from_slice(slot);
        }
        seal(&key, &mut bytes);
        out.push((key.to_vec(), bytes));
    }
    out
}

/// A stored part of a segment, read in place.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PartRef<'a> {
    bytes: &'a [u8],
    span: usize,
    index: usize,
    width: usize,
    data: usize,
}

impl<'a> PartRef<'a> {
    /// The part stored under `key`, checked against its checksum.
    pub(crate) fn parse(key: &[u8], bytes: &'a [u8]) -> Result<Self, GraphError> {
        checked(key, bytes)?;
        Self::trusted(key, bytes)
    }

    /// A part whose checksum was already checked, read without checking it again.
    pub(crate) fn trusted(key: &[u8], bytes: &'a [u8]) -> Result<Self, GraphError> {
        let part = Self::laid_out(key, bytes)?;
        let mut end = 0;
        for at in 0..run_of(part.span, part.index) - 1 {
            let next = part.offset(at);
            if next < end || next > bytes.len() - part.data {
                return Err(DecodeError::UnexpectedEnd.into());
            }
            end = next;
        }
        Ok(part)
    }

    fn verified(key: &[u8], bytes: &'a [u8]) -> Result<Self, GraphError> {
        Self::laid_out(key, bytes)
    }

    fn laid_out(key: &[u8], bytes: &'a [u8]) -> Result<Self, GraphError> {
        let (_, Entry::Part { span, index }) = parse_key(key)? else {
            return Err(BAD_KEY);
        };
        let Some(flags) = bytes.get(FLAGS_AT).copied() else {
            return Err(UNKNOWN_LAYOUT);
        };
        let width = match flags {
            0 => 2,
            WIDE_OFFSETS => 4,
            _ => return Err(UNKNOWN_LAYOUT),
        };
        let run = run_of(span, index);
        let data = OFFSETS_AT + width * (run - 1);
        if bytes.len() <= data {
            return Err(DecodeError::UnexpectedEnd.into());
        }
        if offset_width(bytes.len() - data) != width {
            return Err(NOT_CANONICAL);
        }
        Ok(Self {
            bytes,
            span,
            index,
            width,
            data,
        })
    }

    fn offset(&self, at: usize) -> usize {
        let start = OFFSETS_AT + self.width * at;
        let mut word = [0u8; 4];
        word[..self.width].copy_from_slice(&self.bytes[start..start + self.width]);
        u32::from_le_bytes(word) as usize
    }

    fn slot_bytes(&self, at: usize) -> &'a [u8] {
        let run = run_of(self.span, self.index);
        let start = if at == 0 { 0 } else { self.offset(at - 1) };
        let end = if at + 1 == run {
            self.bytes.len() - self.data
        } else {
            self.offset(at)
        };
        &self.bytes[self.data + start..self.data + end]
    }

    fn encoded(&self, position: usize) -> &'a [u8] {
        self.slot_bytes(position - self.index * self.span)
    }

    /// The positions the part covers whose slots hold relationships.
    pub(crate) fn held(&self) -> impl Iterator<Item = usize> + '_ {
        let start = self.index * self.span;
        (0..run_of(self.span, self.index))
            .filter(|at| !self.slot_bytes(*at).is_empty())
            .map(move |at| start + at)
    }

    /// Calls `visit` with each relationship of the slot at `position`, which the part covers,
    /// ascending by ID.
    pub(crate) fn visit_slot(
        &self,
        position: usize,
        visit: impl FnMut(Edge) -> Result<(), GraphError>,
    ) -> Result<(), GraphError> {
        visit_edges(self.slot_bytes(position - self.index * self.span), visit)
    }

    pub(crate) fn slot(&self, position: usize) -> Result<Vec<Edge>, GraphError> {
        let mut edges = Vec::new();
        self.visit_slot(position, |edge| {
            edges.push(edge);
            Ok(())
        })?;
        Ok(edges)
    }

    /// Each position the part covers with its relationships, for the positions that have some.
    #[cfg(test)]
    pub(crate) fn slots(&self) -> Result<Slots, GraphError> {
        let start = self.index * self.span;
        let mut out = Vec::new();
        for position in start..start + run_of(self.span, self.index) {
            let edges = self.slot(position)?;
            if !edges.is_empty() {
                out.push((position, edges));
            }
        }
        Ok(out)
    }
}

/// What a delta changes in one slot: the relationships of the stored parts it drops, and the
/// ones it adds, both ascending by ID.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Change {
    pub(crate) removed: Vec<RelationshipId>,
    pub(crate) added: Vec<Edge>,
}

impl Change {
    pub(crate) fn is_empty(&self) -> bool {
        self.removed.is_empty() && self.added.is_empty()
    }

    fn encoded_len(&self) -> usize {
        let mut ascending = Ascending::default();
        let removed: usize = self
            .removed
            .iter()
            .map(|relationship| ascending.len(*relationship))
            .sum();
        FLAGS_AT
            + varint_len(self.removed.len() as u64)
            + removed
            + varint_len(self.added.len() as u64)
            + edges_len(&self.added)
    }

    /// The delta stored under `key`.
    pub(crate) fn encode(&self, key: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(self.encoded_len());
        bytes.push(LAYOUT_VERSION);
        bytes.extend_from_slice(&[0; 4]);
        put_varint(&mut bytes, self.removed.len() as u64);
        let mut ascending = Ascending::default();
        for relationship in &self.removed {
            ascending.put(&mut bytes, *relationship);
        }
        put_varint(&mut bytes, self.added.len() as u64);
        put_edges(&mut bytes, &self.added);
        seal(key, &mut bytes);
        bytes
    }

    /// The delta stored under `key`, checked against its checksum. A stored delta changes
    /// something.
    pub(crate) fn parse(key: &[u8], bytes: &[u8]) -> Result<Self, GraphError> {
        checked(key, bytes)?;
        let mut varints = Varints::new(&bytes[FLAGS_AT..]);
        let count = varints.count()?;
        let mut removed = Vec::with_capacity(count);
        let mut ascending = Ascending::default();
        for _ in 0..count {
            removed.push(ascending.next(&mut varints)?);
        }
        let count = varints.count()?;
        let mut added = Vec::with_capacity(count);
        let mut ascending = Ascending::default();
        for _ in 0..count {
            let relationship = ascending.next(&mut varints)?;
            added.push(Edge {
                relationship,
                other: NodeId(varints.next()?),
            });
        }
        varints.finish()?;
        let change = Self { removed, added };
        if change.is_empty() {
            return Err(NOT_CANONICAL);
        }
        Ok(change)
    }
}

/// Streams the stored relationships of one slot, dropping the ones `change` removes and adding
/// the ones it adds, ascending by ID. Each relationship `change` removes must be stored, and
/// none it adds may be.
#[derive(Debug)]
pub(crate) struct Amended<'c> {
    removed: std::slice::Iter<'c, RelationshipId>,
    next_removed: Option<RelationshipId>,
    added: std::iter::Peekable<std::slice::Iter<'c, Edge>>,
}

impl<'c> Amended<'c> {
    pub(crate) fn new(change: Option<&'c Change>) -> Self {
        let (removed, added): (&[RelationshipId], &[Edge]) = match change {
            Some(change) => (&change.removed, &change.added),
            None => (&[], &[]),
        };
        let mut removed = removed.iter();
        let next_removed = removed.next().copied();
        Self {
            removed,
            next_removed,
            added: added.iter().peekable(),
        }
    }

    /// Takes the next stored relationship and emits the added ones before it.
    pub(crate) fn stored(
        &mut self,
        edge: Edge,
        emit: &mut impl FnMut(Edge),
    ) -> Result<(), GraphError> {
        while let Some(added) = self
            .added
            .next_if(|added| added.relationship <= edge.relationship)
        {
            if added.relationship == edge.relationship {
                return Err(DELTA_DISAGREES);
            }
            emit(*added);
        }
        match self.next_removed {
            Some(removed) if removed == edge.relationship => {
                self.next_removed = self.removed.next().copied();
            }
            Some(removed) if removed < edge.relationship => return Err(DELTA_DISAGREES),
            _ => emit(edge),
        }
        Ok(())
    }

    /// Emits the added relationships after the last stored one, and fails when the change
    /// removes one that is not stored.
    pub(crate) fn finish(self, emit: &mut impl FnMut(Edge)) -> Result<(), GraphError> {
        if self.next_removed.is_some() {
            return Err(DELTA_DISAGREES);
        }
        for added in self.added {
            emit(*added);
        }
        Ok(())
    }
}

/// The relationships of `position` as stored under one segment, merged with its delta, while
/// a read visits the segment's entries in key order: the slot's delta before the parts.
#[derive(Debug, Default)]
pub(crate) struct SlotRead {
    change: Option<Change>,
    parts: bool,
    covered: bool,
}

impl SlotRead {
    /// Reads the entry under `key` if it holds `position`, emitting the slot's relationships
    /// once its part is read.
    /// `verified` says whether the entry's checksum was checked before, and is set once this
    /// read checks it.
    pub(crate) fn visit(
        &mut self,
        entry: Entry,
        position: usize,
        (key, value, verified): (&[u8], &[u8], &mut bool),
        emit: &mut impl FnMut(Edge),
    ) -> Result<(), GraphError> {
        match entry {
            Entry::Delta(at) if at == position => {
                if self.parts {
                    return Err(GraphError::Corrupt(
                        "an adjacency delta sorts after its segment",
                    ));
                }
                self.change = Some(Change::parse(key, value)?);
            }
            Entry::Delta(_) => {}
            Entry::Part { .. } => {
                self.parts = true;
                if entry.covers(position) {
                    if self.covered {
                        return Err(SPANS_DISAGREE);
                    }
                    self.covered = true;
                    let part = if *verified {
                        PartRef::verified(key, value)?
                    } else {
                        let part = PartRef::parse(key, value)?;
                        *verified = true;
                        part
                    };
                    match &self.change {
                        None => part.visit_slot(position, |edge| {
                            emit(edge);
                            Ok(())
                        })?,
                        Some(change) => {
                            let mut amended = Amended::new(Some(change));
                            part.visit_slot(position, |edge| amended.stored(edge, emit))?;
                            amended.finish(emit)?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Ends the segment, emitting what its delta adds when no part covers the slot.
    pub(crate) fn finish(self, emit: &mut impl FnMut(Edge)) -> Result<(), GraphError> {
        match (&self.change, self.parts, self.covered) {
            (Some(_), false, _) => Err(DELTA_WITHOUT_PARTS),
            (Some(change), true, false) => Amended::new(Some(change)).finish(emit),
            _ => Ok(()),
        }
    }
}

/// One segment of one relationship type in a write transaction: its stored parts and deltas,
/// and what this transaction changed, kept as each slot's change against the stored parts.
#[derive(Clone, Debug, Default)]
pub(crate) struct SegmentEdit {
    parts: Vec<Held>,
    part_at: Vec<u8>,
    span: usize,
    changes: BTreeMap<usize, Change>,
    stored_deltas: BTreeSet<usize>,
    touched: BTreeSet<usize>,
    stored_slots: BTreeMap<usize, Vec<Edge>>,
}

/// What commit writes for one edited segment: the entries to store and the keys to delete.
#[derive(Debug, Default)]
pub(crate) struct Flush {
    pub(crate) put: Vec<Stored>,
    pub(crate) delete: Vec<Vec<u8>>,
}

impl SegmentEdit {
    /// The segment as stored, from its entries in key order, each checked against its
    /// checksum. Every delta must amend the parts, which must agree on their span.
    pub(crate) fn load<'e>(
        id: &SegmentId,
        entries: impl IntoIterator<Item = (&'e [u8], &'e [u8])>,
    ) -> Result<Self, GraphError> {
        let mut edit = Self::default();
        for (key, value) in entries {
            let (held, entry) = parse_key(key)?;
            if held != *id {
                return Err(BAD_KEY);
            }
            match entry {
                Entry::Delta(position) => {
                    if !edit.parts.is_empty() {
                        return Err(GraphError::Corrupt(
                            "an adjacency delta sorts after its segment",
                        ));
                    }
                    edit.changes.insert(position, Change::parse(key, value)?);
                    edit.stored_deltas.insert(position);
                }
                Entry::Part { span, index } => {
                    PartRef::parse(key, value)?;
                    if edit.span != 0 && edit.span != span {
                        return Err(SPANS_DISAGREE);
                    }
                    edit.span = span;
                    edit.part_at.resize(SEGMENT_SLOTS, u8::MAX);
                    edit.part_at[index] = edit.parts.len() as u8;
                    edit.parts.push((key.into(), value.into()));
                }
            }
        }
        if edit.parts.is_empty() && !edit.changes.is_empty() {
            return Err(DELTA_WITHOUT_PARTS);
        }
        for position in edit.changes.keys() {
            edit.visit(*position, &mut |_| {})?;
        }
        Ok(edit)
    }

    fn part_of(&self, position: usize) -> Option<&Held> {
        let index = position.checked_div(self.span)?;
        let at = self.part_at.get(index)?;
        self.parts.get(usize::from(*at))
    }

    fn stored_cached(&mut self, position: usize) -> Result<&[Edge], GraphError> {
        if !self.stored_slots.contains_key(&position) {
            let edges = match self.part_of(position) {
                Some((key, bytes)) => PartRef::trusted(key, bytes)?.slot(position)?,
                None => Vec::new(),
            };
            self.stored_slots.insert(position, edges);
        }
        Ok(self.stored_slots.get(&position).map_or(&[], Vec::as_slice))
    }

    /// Calls `emit` with each relationship of `position` as the transaction sees it, ascending
    /// by ID.
    pub(crate) fn visit(
        &self,
        position: usize,
        emit: &mut impl FnMut(Edge),
    ) -> Result<(), GraphError> {
        let mut amended = Amended::new(self.changes.get(&position));
        if let Some(edges) = self.stored_slots.get(&position) {
            for edge in edges {
                amended.stored(*edge, emit)?;
            }
        } else if let Some((key, bytes)) = self.part_of(position) {
            PartRef::trusted(key, bytes)?
                .visit_slot(position, |edge| amended.stored(edge, emit))?;
        }
        amended.finish(emit)
    }

    pub(crate) fn add(&mut self, position: usize, edge: Edge) {
        self.touched.insert(position);
        let added = &mut self.changes.entry(position).or_default().added;
        let at = added.partition_point(|held| held.relationship < edge.relationship);
        added.insert(at, edge);
    }

    /// Drops relationship `relationship` from `position`, which must hold it.
    pub(crate) fn remove(
        &mut self,
        position: usize,
        relationship: RelationshipId,
    ) -> Result<(), GraphError> {
        self.touched.insert(position);
        if let Some(change) = self.changes.get_mut(&position)
            && let Ok(at) = change
                .added
                .binary_search_by_key(&relationship, |edge| edge.relationship)
        {
            change.added.remove(at);
            return Ok(());
        }
        let stored = self
            .stored_cached(position)?
            .binary_search_by_key(&relationship, |edge| edge.relationship)
            .is_ok();
        let removed = &mut self.changes.entry(position).or_default().removed;
        match removed.binary_search(&relationship) {
            Err(at) if stored => {
                removed.insert(at, relationship);
                Ok(())
            }
            _ => Err(GraphError::Corrupt(
                "a relationship is missing from the adjacency of its node",
            )),
        }
    }

    #[cfg(test)]
    /// Every slot of the segment as the transaction sees it, for the positions that have
    /// relationships.
    pub(crate) fn slots(&self) -> Result<Slots, GraphError> {
        let mut positions: Vec<usize> = self.changes.keys().copied().collect();
        for (key, bytes) in &self.parts {
            positions.extend(PartRef::trusted(key, bytes)?.held());
        }
        positions.sort_unstable();
        positions.dedup();
        let mut out = Vec::with_capacity(positions.len());
        for position in positions {
            let mut edges = Vec::new();
            self.visit(position, &mut |edge| edges.push(edge))?;
            if !edges.is_empty() {
                out.push((position, edges));
            }
        }
        Ok(out)
    }

    fn may_be_emptied(&self) -> Result<bool, GraphError> {
        if self.changes.values().any(|change| !change.added.is_empty()) {
            return Ok(false);
        }
        for (key, bytes) in &self.parts {
            if PartRef::trusted(key, bytes)?
                .held()
                .any(|position| !self.changes.contains_key(&position))
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// What commit stores: deltas beside the stored parts when the segment has parts that keep
    /// a relationship and at most `MAX_DELTAS` deltas, none over `DELTA_BYTES`. Otherwise the
    /// segment is laid out again, with the slots no change touched copied as they are stored,
    /// and only the parts that differ from the stored ones are written.
    pub(crate) fn flush(self, id: &SegmentId) -> Result<Flush, GraphError> {
        let deltas = self
            .changes
            .values()
            .filter(|change| !change.is_empty())
            .count();
        let oversized = self
            .touched
            .iter()
            .filter_map(|position| self.changes.get(position))
            .any(|change| change.encoded_len() > DELTA_BYTES);
        let rewrite = self.parts.is_empty() || deltas > MAX_DELTAS || oversized;
        if rewrite || self.may_be_emptied()? {
            let parts = self.lay_out_again(id)?;
            if rewrite || parts.is_empty() {
                return Ok(self.replace(id, parts));
            }
        }
        let mut flush = Flush::default();
        for position in &self.touched {
            let key = id.delta_key(*position);
            match self.changes.get(position) {
                Some(change) if !change.is_empty() => {
                    flush.put.push((key.to_vec(), change.encode(&key)));
                }
                _ if self.stored_deltas.contains(position) => flush.delete.push(key.to_vec()),
                _ => {}
            }
        }
        Ok(flush)
    }

    fn lay_out_again(&self, id: &SegmentId) -> Result<Vec<Stored>, GraphError> {
        let mut positions: Vec<usize> = self.changes.keys().copied().collect();
        for (key, bytes) in &self.parts {
            positions.extend(PartRef::trusted(key, bytes)?.held());
        }
        positions.sort_unstable();
        positions.dedup();
        let mut changed = Vec::new();
        for position in positions
            .iter()
            .filter(|position| self.changes.contains_key(position))
        {
            let mut edges = Vec::new();
            self.visit(*position, &mut |edge| edges.push(edge))?;
            let mut bytes = Vec::with_capacity(edges_len(&edges));
            put_edges(&mut bytes, &edges);
            changed.push((*position, bytes));
        }
        let mut slots: Vec<(usize, &[u8])> = Vec::with_capacity(positions.len());
        let mut changed_slots = changed.iter().peekable();
        for position in positions {
            let bytes = match changed_slots.next_if(|(at, _)| *at == position) {
                Some((_, bytes)) => bytes.as_slice(),
                None => match self.part_of(position) {
                    Some((key, bytes)) => PartRef::trusted(key, bytes)?.encoded(position),
                    None => &[],
                },
            };
            if !bytes.is_empty() {
                slots.push((position, bytes));
            }
        }
        Ok(lay_out(id, &slots, None))
    }

    fn replace(&self, id: &SegmentId, parts: Vec<Stored>) -> Flush {
        let mut flush = Flush::default();
        for position in &self.stored_deltas {
            flush.delete.push(id.delta_key(*position).to_vec());
        }
        for (key, _) in &self.parts {
            if !parts.iter().any(|(new, _)| new.as_slice() == key.as_ref()) {
                flush.delete.push(key.to_vec());
            }
        }
        for (key, bytes) in parts {
            let same = self.parts.iter().any(|(held, stored)| {
                held.as_ref() == key.as_slice() && stored.as_ref() == bytes.as_slice()
            });
            if !same {
                flush.put.push((key, bytes));
            }
        }
        flush
    }
}

/// The stored entries of the adjacency segments a query read last, one per side, so a scan that
/// expands consecutive nodes reads each segment from storage once instead of once per node.
#[derive(Debug, Default)]
pub(crate) struct AdjacencyReader {
    sides: [Recent; 2],
}

#[derive(Debug, Default)]
struct Recent {
    prefix: Vec<u8>,
    entries: Option<Vec<Verified>>,
}

/// A stored entry a reader holds, and whether a read checked its checksum.
pub(crate) type Verified = (Box<[u8]>, Box<[u8]>, bool);

/// What [`AdjacencyReader::entries`] gives for one read of a prefix.
pub(crate) enum Entries<'r> {
    /// The prefix was read before, so its entries are held.
    Held(&'r mut [Verified]),
    /// The prefix was not read just before, so it is read from storage in place.
    Stored,
}

impl AdjacencyReader {
    /// The entries of `prefix` on `side`, held once the prefix is read twice in a row. `load`
    /// reads them from storage.
    pub(crate) fn entries<E>(
        &mut self,
        side: Side,
        prefix: &[u8],
        load: impl FnOnce() -> Result<Vec<Verified>, E>,
    ) -> Result<Entries<'_>, E> {
        let recent = &mut self.sides[usize::from(side.byte())];
        if recent.prefix != prefix {
            recent.prefix.clear();
            recent.prefix.extend_from_slice(prefix);
            recent.entries = None;
            return Ok(Entries::Stored);
        }
        if recent.entries.is_none() {
            recent.entries = Some(load()?);
        }
        Ok(match &mut recent.entries {
            Some(entries) => Entries::Held(entries),
            None => Entries::Stored,
        })
    }
}

#[cfg(test)]
#[path = "adjacency.test.rs"]
mod tests;
