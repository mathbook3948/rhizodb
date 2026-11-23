use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::PoisonError;
use std::sync::RwLock;

use crate::value::DecodeError;

use super::GraphError;
use super::ids::Label;
use super::ids::NodeId;
use super::ids::Token;

const GROUP_SHIFT: u32 = 12;
/// How many consecutive node IDs one row group covers.
pub(crate) const SLOTS: usize = 1 << GROUP_SHIFT;
const WORDS: usize = SLOTS / 64;
const BITMAP_BYTES: usize = SLOTS / 8;
const LAYOUT_VERSION: u8 = 1;
const META_KIND: u8 = 0;
const HEADER: usize = 1 + BITMAP_BYTES + 2;

/// The row group holding `node` and the node's slot in it.
pub(crate) fn locate(node: NodeId) -> (u64, usize) {
    (
        node.0 >> GROUP_SHIFT,
        (node.0 & (SLOTS as u64 - 1)) as usize,
    )
}

pub(crate) fn node_at(group: u64, slot: usize) -> NodeId {
    NodeId((group << GROUP_SHIFT) | slot as u64)
}

/// The key of a row group's node meta chunk: the group, then the chunk kind and a zero part.
pub(crate) fn meta_key(group: u64) -> [u8; 13] {
    let mut key = [0u8; 13];
    key[..8].copy_from_slice(&group.to_be_bytes());
    key[8] = META_KIND;
    key
}

/// The group of a node meta chunk key, or `None` for a chunk of another kind.
pub(crate) fn parse_meta_key(key: &[u8]) -> Result<Option<u64>, DecodeError> {
    let key = <[u8; 13]>::try_from(key).map_err(|_| DecodeError::UnexpectedEnd)?;
    if key[8] != META_KIND {
        return Ok(None);
    }
    if key[9..] != [0; 4] {
        return Err(DecodeError::TrailingBytes);
    }
    let mut group = [0u8; 8];
    group.copy_from_slice(&key[..8]);
    Ok(Some(u64::from_be_bytes(group)))
}

/// An interned set of labels. ID 0 is the empty set, which has no stored entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct LabelSetId(pub(crate) u32);

impl LabelSetId {
    pub(crate) const EMPTY: Self = Self(0);
}

/// The labels of a label set, ascending, as the key of the label set table.
pub(crate) fn label_set_key(labels: &[Token<Label>]) -> Vec<u8> {
    labels
        .iter()
        .flat_map(|label| label.id().to_be_bytes())
        .collect()
}

/// The labels a label set key or value holds, which must ascend without repeats.
pub(crate) fn parse_label_set(bytes: &[u8]) -> Result<Arc<[Token<Label>]>, GraphError> {
    let (words, rest) = bytes.as_chunks::<4>();
    if !rest.is_empty() {
        return Err(DecodeError::UnexpectedEnd.into());
    }
    let labels: Arc<[Token<Label>]> = words
        .iter()
        .map(|word| Token::new(u32::from_be_bytes(*word)))
        .collect();
    if labels.is_empty() || !labels.is_sorted_by(|a, b| a < b) {
        return Err(GraphError::Corrupt("a label set is not ascending"));
    }
    Ok(labels)
}

/// Label sets by ID and by key. A label set never changes once interned, so the committed ones
/// can be shared by every transaction.
#[derive(Debug, Default)]
pub(crate) struct LabelSetCache {
    by_id: RwLock<HashMap<u32, Arc<[Token<Label>]>>>,
    by_key: RwLock<HashMap<Vec<u8>, u32>>,
}

impl LabelSetCache {
    pub(crate) fn labels(&self, set: LabelSetId) -> Option<Arc<[Token<Label>]>> {
        let by_id = self.by_id.read().unwrap_or_else(PoisonError::into_inner);
        by_id.get(&set.0).cloned()
    }

    pub(crate) fn id(&self, key: &[u8]) -> Option<LabelSetId> {
        let by_key = self.by_key.read().unwrap_or_else(PoisonError::into_inner);
        by_key.get(key).copied().map(LabelSetId)
    }

    pub(crate) fn insert(&self, set: LabelSetId, labels: Arc<[Token<Label>]>) {
        let key = label_set_key(&labels);
        self.by_id
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(set.0, labels);
        self.by_key
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key, set.0);
    }
}

/// The node meta of one row group in a form a write edits: which slots hold a live node and
/// each live node's label set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MetaEdit {
    alive: [u64; WORDS],
    sets: Vec<LabelSetId>,
}

impl Default for MetaEdit {
    fn default() -> Self {
        Self {
            alive: [0; WORDS],
            sets: vec![LabelSetId::EMPTY; SLOTS],
        }
    }
}

impl MetaEdit {
    pub(crate) fn label_set(&self, slot: usize) -> Option<LabelSetId> {
        if self.alive[slot / 64] & (1 << (slot % 64)) == 0 {
            return None;
        }
        self.sets.get(slot).copied()
    }

    /// Marks the slot live with the label set `set`.
    pub(crate) fn put(&mut self, slot: usize, set: LabelSetId) {
        self.alive[slot / 64] |= 1 << (slot % 64);
        self.sets[slot] = set;
    }

    pub(crate) fn remove(&mut self, slot: usize) {
        self.alive[slot / 64] &= !(1 << (slot % 64));
        self.sets[slot] = LabelSetId::EMPTY;
    }

    pub(crate) fn live(&self) -> impl Iterator<Item = (usize, LabelSetId)> + '_ {
        (0..SLOTS).filter_map(|slot| Some((slot, self.label_set(slot)?)))
    }

    /// The stored chunk, or `None` when no slot is live and the group has no chunk. `labels`
    /// gives the labels of each label set the group uses.
    pub(crate) fn encode(
        &self,
        mut labels: impl FnMut(LabelSetId) -> Result<Arc<[Token<Label>]>, GraphError>,
    ) -> Result<Option<Vec<u8>>, GraphError> {
        let mut uses: BTreeMap<LabelSetId, u16> = BTreeMap::new();
        for (_, set) in self.live() {
            *uses.entry(set).or_default() += 1;
        }
        if uses.is_empty() {
            return Ok(None);
        }
        let dictionary: Vec<LabelSetId> = uses.keys().copied().collect();
        let width = code_width(dictionary.len());
        let mut out = Vec::with_capacity(HEADER + 4 * dictionary.len() + width * SLOTS);
        out.push(LAYOUT_VERSION);
        for word in self.alive {
            out.extend_from_slice(&word.to_le_bytes());
        }
        out.extend_from_slice(&(dictionary.len() as u16).to_be_bytes());
        for set in &dictionary {
            out.extend_from_slice(&set.0.to_be_bytes());
        }
        if width > 0 {
            for slot in 0..SLOTS {
                let code = match self.label_set(slot) {
                    Some(set) => dictionary.binary_search(&set).unwrap_or_default(),
                    None => 0,
                };
                out.extend_from_slice(&(code as u16).to_be_bytes()[2 - width..]);
            }
        }
        let mut counts: BTreeMap<Token<Label>, u16> = BTreeMap::new();
        for (set, used) in uses {
            for label in labels(set)?.iter() {
                *counts.entry(*label).or_default() += used;
            }
        }
        out.extend_from_slice(&(counts.len() as u32).to_be_bytes());
        for (label, count) in counts {
            out.extend_from_slice(&label.id().to_be_bytes());
            out.extend_from_slice(&count.to_be_bytes());
        }
        Ok(Some(out))
    }
}

fn code_width(dictionary: usize) -> usize {
    match dictionary {
        0 | 1 => 0,
        2..=256 => 1,
        _ => 2,
    }
}

/// A stored node meta chunk, read in place.
#[derive(Clone, Copy, Debug)]
pub(crate) struct MetaBytes<'a> {
    bytes: &'a [u8],
    dictionary: usize,
    width: usize,
    counts: usize,
    labels: usize,
}

impl<'a> MetaBytes<'a> {
    pub(crate) fn parse(bytes: &'a [u8]) -> Result<Self, DecodeError> {
        if bytes.first() != Some(&LAYOUT_VERSION) || bytes.len() < HEADER {
            return Err(DecodeError::UnexpectedEnd);
        }
        let dictionary = usize::from(u16::from_be_bytes([bytes[HEADER - 2], bytes[HEADER - 1]]));
        let width = code_width(dictionary);
        let counts = HEADER + 4 * dictionary + width * SLOTS;
        let labels = bytes
            .get(counts..counts + 4)
            .map(|count| u32::from_be_bytes([count[0], count[1], count[2], count[3]]))
            .ok_or(DecodeError::UnexpectedEnd)?;
        let labels = usize::try_from(labels).map_err(|_| DecodeError::LengthOverflow)?;
        let end = labels
            .checked_mul(6)
            .and_then(|size| size.checked_add(counts + 4))
            .ok_or(DecodeError::LengthOverflow)?;
        match end.cmp(&bytes.len()) {
            std::cmp::Ordering::Less => Err(DecodeError::TrailingBytes),
            std::cmp::Ordering::Greater => Err(DecodeError::UnexpectedEnd),
            std::cmp::Ordering::Equal => Ok(Self {
                bytes,
                dictionary,
                width,
                counts,
                labels,
            }),
        }
    }

    fn alive(&self, slot: usize) -> bool {
        self.bytes[1 + slot / 8] & (1 << (slot % 8)) != 0
    }

    /// The position in the dictionary of the label set of the node in `slot`, if it is live.
    pub(crate) fn code(&self, slot: usize) -> Option<usize> {
        if !self.alive(slot) {
            return None;
        }
        let at = HEADER + 4 * self.dictionary + self.width * slot;
        Some(match self.width {
            0 => 0,
            1 => usize::from(self.bytes[at]),
            _ => usize::from(u16::from_be_bytes([self.bytes[at], self.bytes[at + 1]])),
        })
    }

    /// The label sets the group uses, in code order.
    pub(crate) fn dictionary(&self) -> impl Iterator<Item = LabelSetId> + '_ {
        (0..self.dictionary).map(|index| {
            let at = HEADER + 4 * index;
            LabelSetId(u32::from_be_bytes([
                self.bytes[at],
                self.bytes[at + 1],
                self.bytes[at + 2],
                self.bytes[at + 3],
            ]))
        })
    }

    pub(crate) fn label_set(&self, slot: usize) -> Option<LabelSetId> {
        let code = self.code(slot)?;
        self.dictionary().nth(code)
    }

    /// How many live nodes of the group carry `label`.
    pub(crate) fn label_count(&self, label: Token<Label>) -> u64 {
        let start = self.counts + 4;
        let entry = |index: usize| {
            let at = start + 6 * index;
            let id = u32::from_be_bytes([
                self.bytes[at],
                self.bytes[at + 1],
                self.bytes[at + 2],
                self.bytes[at + 3],
            ]);
            let count = u16::from_be_bytes([self.bytes[at + 4], self.bytes[at + 5]]);
            (id, count)
        };
        let (mut low, mut high) = (0, self.labels);
        while low < high {
            let middle = low + (high - low) / 2;
            let (id, count) = entry(middle);
            match id.cmp(&label.id()) {
                std::cmp::Ordering::Equal => return u64::from(count),
                std::cmp::Ordering::Less => low = middle + 1,
                std::cmp::Ordering::Greater => high = middle,
            }
        }
        0
    }

    pub(crate) fn to_edit(self) -> Result<MetaEdit, GraphError> {
        let dictionary: Vec<LabelSetId> = self.dictionary().collect();
        let mut edit = MetaEdit::default();
        for slot in 0..SLOTS {
            if let Some(code) = self.code(slot) {
                let set = dictionary.get(code).ok_or(GraphError::Corrupt(
                    "a node meta code is outside its dictionary",
                ))?;
                edit.put(slot, *set);
            }
        }
        Ok(edit)
    }
}

/// The node meta of one group as a read sees it: stored, or edited by the write transaction.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Meta<'a> {
    Stored(MetaBytes<'a>),
    Edited(&'a MetaEdit),
}

impl Meta<'_> {
    pub(crate) fn label_set(&self, slot: usize) -> Option<LabelSetId> {
        match self {
            Meta::Stored(bytes) => bytes.label_set(slot),
            Meta::Edited(edit) => edit.label_set(slot),
        }
    }
}

#[derive(Debug)]
struct CachedMeta {
    bytes: Box<[u8]>,
    sets: Vec<Arc<[Token<Label>]>>,
}

/// The node meta chunks one query read so far, so testing a node's labels reads storage once
/// per group.
#[derive(Debug, Default)]
pub(crate) struct LabelReader {
    groups: Vec<(u64, Option<CachedMeta>)>,
    last: usize,
}

/// A stored node meta chunk and the labels of each set in its dictionary.
pub(crate) type Loaded = (Box<[u8]>, Vec<Arc<[Token<Label>]>>);

/// Where a label reader finds a group.
pub(crate) enum Cached<'a> {
    Absent,
    Found {
        meta: MetaBytes<'a>,
        sets: &'a [Arc<[Token<Label>]>],
    },
}

impl LabelReader {
    /// The cached chunk of `group`, loading it with `load` on a miss. `load` returns the
    /// stored bytes and the labels of each set in its dictionary.
    pub(crate) fn group(
        &mut self,
        group: u64,
        load: impl FnOnce() -> Result<Option<Loaded>, GraphError>,
    ) -> Result<Cached<'_>, GraphError> {
        let index = match self.groups.get(self.last) {
            Some((held, _)) if *held == group => self.last,
            _ => match self.groups.binary_search_by_key(&group, |(held, _)| *held) {
                Ok(index) => index,
                Err(index) => {
                    let loaded = load()?.map(|(bytes, sets)| CachedMeta { bytes, sets });
                    self.groups.insert(index, (group, loaded));
                    index
                }
            },
        };
        self.last = index;
        match self.groups.get(index) {
            Some((_, Some(cached))) => Ok(Cached::Found {
                meta: MetaBytes::parse(&cached.bytes)?,
                sets: &cached.sets,
            }),
            _ => Ok(Cached::Absent),
        }
    }
}

#[cfg(test)]
#[path = "meta.test.rs"]
mod tests;
