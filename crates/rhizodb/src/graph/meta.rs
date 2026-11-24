use std::cell::OnceCell;
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
const HEAD_CHECKSUM: usize = 1;
const BODY_CHECKSUM: usize = 5;
const GROUP_AT: usize = 9;
const LIVE: usize = 17;
const DICTIONARY: usize = 19;
const KEY: usize = 13;
const STATE: usize = 5;
const DELTA: usize = 2 * STATE + 4;
/// The most deltas a group keeps beside its chunk. A commit that would leave more rewrites
/// the chunk and drops them.
pub(crate) const MAX_DELTAS: usize = 64;

const CHECKSUM_MISMATCH: GraphError =
    GraphError::Corrupt("a node meta entry does not match its checksum");
const CODE_OUTSIDE: GraphError = GraphError::Corrupt("a node meta code is outside its dictionary");
const DELTA_WITHOUT_CHUNK: GraphError = GraphError::Corrupt("a node meta delta has no chunk");
const DELTA_DISAGREES: GraphError =
    GraphError::Corrupt("a node meta delta does not amend its chunk");

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

fn key(group: u64, part: u32) -> [u8; KEY] {
    let mut key = [0u8; KEY];
    key[..8].copy_from_slice(&group.to_be_bytes());
    key[8] = META_KIND;
    key[9..].copy_from_slice(&part.to_be_bytes());
    key
}

/// The key of a row group's node meta chunk: the group, the chunk kind, and part 0.
pub(crate) fn meta_key(group: u64) -> [u8; KEY] {
    key(group, 0)
}

/// The key of the delta that amends the chunk of `group` at `slot`: part `1 + slot`, so a
/// group's deltas follow its chunk in slot order.
pub(crate) fn delta_key(group: u64, slot: usize) -> [u8; KEY] {
    key(group, 1 + slot as u32)
}

/// The prefix of every node meta entry of `group`.
pub(crate) fn group_prefix(group: u64) -> [u8; 9] {
    let mut prefix = [0u8; 9];
    prefix.copy_from_slice(&key(group, 0)[..9]);
    prefix
}

/// What a node meta key names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Part {
    Chunk,
    Delta(usize),
}

/// The group and part of a node meta key, or `None` for an entry of another kind.
pub(crate) fn parse_key(key: &[u8]) -> Result<Option<(u64, Part)>, GraphError> {
    let key = <[u8; KEY]>::try_from(key).map_err(|_| DecodeError::UnexpectedEnd)?;
    if key[8] != META_KIND {
        return Ok(None);
    }
    let mut group = [0u8; 8];
    group.copy_from_slice(&key[..8]);
    let group = u64::from_be_bytes(group);
    let part = u32::from_be_bytes([key[9], key[10], key[11], key[12]]) as usize;
    match part {
        0 => Ok(Some((group, Part::Chunk))),
        1..=SLOTS => Ok(Some((group, Part::Delta(part - 1)))),
        _ => Err(GraphError::Corrupt("a node meta key names no part")),
    }
}

fn checksum(key: &[u8], parts: &[&[u8]]) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(key);
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize()
}

fn stored_checksum(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
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

    pub(crate) fn remove(&self, set: LabelSetId) {
        let removed = self
            .by_id
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&set.0);
        if let Some(labels) = removed {
            self.by_key
                .write()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&label_set_key(&labels));
        }
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

/// A slot's state: the label set of its live node, or `None` when the slot holds no node.
pub(crate) type Slot = Option<LabelSetId>;

fn encode_state(state: Slot) -> [u8; STATE] {
    let mut bytes = [0u8; STATE];
    if let Some(set) = state {
        bytes[0] = 1;
        bytes[1..].copy_from_slice(&set.0.to_be_bytes());
    }
    bytes
}

fn parse_state(bytes: &[u8]) -> Result<Slot, GraphError> {
    let set = u32::from_be_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]);
    match (bytes[0], set) {
        (0, 0) => Ok(None),
        (1, set) => Ok(Some(LabelSetId(set))),
        _ => Err(GraphError::Corrupt("a node meta delta holds no slot state")),
    }
}

/// A change to one slot of a group that its stored chunk does not hold yet. `before` is the
/// slot as the chunk holds it, so a reader can apply the delta without the chunk at hand.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Delta {
    pub(crate) slot: usize,
    pub(crate) before: Slot,
    pub(crate) after: Slot,
}

impl Delta {
    pub(crate) fn encode(&self, group: u64) -> [u8; DELTA] {
        let key = delta_key(group, self.slot);
        let before = encode_state(self.before);
        let after = encode_state(self.after);
        let mut bytes = [0u8; DELTA];
        bytes[..STATE].copy_from_slice(&before);
        bytes[STATE..2 * STATE].copy_from_slice(&after);
        bytes[2 * STATE..].copy_from_slice(&checksum(&key, &[&before, &after]).to_le_bytes());
        bytes
    }

    /// The delta stored under `key`, which must name a delta, checked against its checksum.
    pub(crate) fn parse(key: &[u8], bytes: &[u8]) -> Result<Self, GraphError> {
        let Some((_, Part::Delta(slot))) = parse_key(key)? else {
            return Err(GraphError::Corrupt("a node meta key names no delta"));
        };
        if bytes.len() != DELTA {
            return Err(DecodeError::UnexpectedEnd.into());
        }
        if checksum(key, &[&bytes[..2 * STATE]]) != stored_checksum(bytes, 2 * STATE) {
            return Err(CHECKSUM_MISMATCH);
        }
        Ok(Self {
            slot,
            before: parse_state(&bytes[..STATE])?,
            after: parse_state(&bytes[STATE..2 * STATE])?,
        })
    }
}

/// The node meta of one row group in a form a write edits: which slots hold a live node, each
/// live node's label set, and how many live nodes use each label set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MetaEdit {
    alive: [u64; WORDS],
    sets: Vec<LabelSetId>,
    uses: BTreeMap<LabelSetId, u32>,
}

impl Default for MetaEdit {
    fn default() -> Self {
        Self {
            alive: [0; WORDS],
            sets: vec![LabelSetId::EMPTY; SLOTS],
            uses: BTreeMap::new(),
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
        self.remove(slot);
        self.alive[slot / 64] |= 1 << (slot % 64);
        self.sets[slot] = set;
        *self.uses.entry(set).or_insert(0) += 1;
    }

    pub(crate) fn remove(&mut self, slot: usize) {
        if let Some(set) = self.label_set(slot)
            && let Some(uses) = self.uses.get_mut(&set)
        {
            *uses -= 1;
            if *uses == 0 {
                self.uses.remove(&set);
            }
        }
        self.alive[slot / 64] &= !(1 << (slot % 64));
        self.sets[slot] = LabelSetId::EMPTY;
    }

    /// Each label set a live node of the group has, with how many live nodes have it.
    pub(crate) fn uses(&self) -> impl Iterator<Item = (LabelSetId, u32)> + '_ {
        self.uses.iter().map(|(set, uses)| (*set, *uses))
    }

    pub(crate) fn set(&mut self, slot: usize, state: Slot) {
        match state {
            Some(set) => self.put(slot, set),
            None => self.remove(slot),
        }
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.alive.iter().all(|word| *word == 0)
    }

    pub(crate) fn live(&self) -> impl Iterator<Item = (usize, LabelSetId)> + '_ {
        ones(self.alive).map(|slot| (slot, self.sets[slot]))
    }

    /// The stored chunk of `group`, or `None` when no slot is live and the group has no chunk.
    /// `labels` gives the labels of each label set the group uses.
    pub(crate) fn encode(
        &self,
        group: u64,
        mut labels: impl FnMut(LabelSetId) -> Result<Arc<[Token<Label>]>, GraphError>,
    ) -> Result<Option<Vec<u8>>, GraphError> {
        let mut uses: Vec<(LabelSetId, u16)> = Vec::new();
        let mut last = 0;
        for (_, set) in self.live() {
            if uses.get(last).is_none_or(|(held, _)| *held != set) {
                last = match uses.iter().position(|(held, _)| *held == set) {
                    Some(found) => found,
                    None => {
                        uses.push((set, 0));
                        uses.len() - 1
                    }
                };
            }
            if let Some((_, used)) = uses.get_mut(last) {
                *used += 1;
            }
        }
        if uses.is_empty() {
            return Ok(None);
        }
        uses.sort_unstable();
        let live: u16 = uses.iter().map(|(_, used)| *used).sum();
        let dictionary: Vec<LabelSetId> = uses.iter().map(|(set, _)| *set).collect();
        let width = code_width(dictionary.len());
        let mut counts: BTreeMap<Token<Label>, u16> = BTreeMap::new();
        for (set, used) in uses {
            for label in labels(set)?.iter() {
                *counts.entry(*label).or_default() += used;
            }
        }
        let body_start = DICTIONARY + 2 + 4 * dictionary.len() + 4 + 6 * counts.len();
        let mut out = Vec::with_capacity(body_start + BITMAP_BYTES + width * SLOTS);
        out.push(LAYOUT_VERSION);
        out.extend_from_slice(&[0; 8]);
        out.extend_from_slice(&group.to_be_bytes());
        out.extend_from_slice(&live.to_be_bytes());
        out.extend_from_slice(&(dictionary.len() as u16).to_be_bytes());
        for set in &dictionary {
            out.extend_from_slice(&set.0.to_be_bytes());
        }
        out.extend_from_slice(&(counts.len() as u32).to_be_bytes());
        for (label, count) in counts {
            out.extend_from_slice(&label.id().to_be_bytes());
            out.extend_from_slice(&count.to_be_bytes());
        }
        for word in self.alive {
            out.extend_from_slice(&word.to_le_bytes());
        }
        if width > 0 {
            let start = out.len();
            out.resize(start + width * SLOTS, 0);
            for (slot, set) in self.live() {
                let code = dictionary.binary_search(&set).unwrap_or_default() as u16;
                let at = start + width * slot;
                out[at..at + width].copy_from_slice(&code.to_be_bytes()[2 - width..]);
            }
        }
        seal(&mut out, body_start);
        Ok(Some(out))
    }
}

fn ones(words: [u64; WORDS]) -> impl Iterator<Item = usize> {
    words.into_iter().enumerate().flat_map(|(index, word)| {
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

fn code_width(dictionary: usize) -> usize {
    match dictionary {
        0 | 1 => 0,
        2..=256 => 1,
        _ => 2,
    }
}

#[derive(Clone, Copy, Debug)]
struct Layout {
    dictionary: usize,
    width: usize,
    counts: usize,
    labels: usize,
    body: usize,
}

impl Layout {
    fn of(bytes: &[u8]) -> Result<Self, GraphError> {
        if bytes.first() != Some(&LAYOUT_VERSION) || bytes.len() < DICTIONARY + 2 {
            return Err(GraphError::Corrupt(
                "a node meta chunk has an unknown layout",
            ));
        }
        let dictionary = usize::from(u16::from_be_bytes([
            bytes[DICTIONARY],
            bytes[DICTIONARY + 1],
        ]));
        let width = code_width(dictionary);
        let counts = DICTIONARY + 2 + 4 * dictionary;
        let labels = bytes
            .get(counts..counts + 4)
            .map(|count| u32::from_be_bytes([count[0], count[1], count[2], count[3]]))
            .ok_or(DecodeError::UnexpectedEnd)?;
        let labels = usize::try_from(labels).map_err(|_| DecodeError::LengthOverflow)?;
        let body = labels
            .checked_mul(6)
            .and_then(|size| size.checked_add(counts + 4))
            .ok_or(DecodeError::LengthOverflow)?;
        let end = body + BITMAP_BYTES + width * SLOTS;
        match end.cmp(&bytes.len()) {
            std::cmp::Ordering::Less => Err(DecodeError::TrailingBytes.into()),
            std::cmp::Ordering::Greater => Err(DecodeError::UnexpectedEnd.into()),
            std::cmp::Ordering::Equal => Ok(Self {
                dictionary,
                width,
                counts,
                labels,
                body,
            }),
        }
    }
}

/// The head of a stored node meta chunk, checked against its own checksum only: enough to
/// read the label counts and the dictionary.
#[derive(Clone, Copy, Debug)]
pub(crate) struct MetaHead<'a> {
    bytes: &'a [u8],
    layout: Layout,
}

impl<'a> MetaHead<'a> {
    pub(crate) fn parse(group: u64, bytes: &'a [u8]) -> Result<Self, GraphError> {
        let layout = Layout::of(bytes)?;
        if crc32fast::hash(&bytes[BODY_CHECKSUM..layout.body])
            != stored_checksum(bytes, HEAD_CHECKSUM)
        {
            return Err(CHECKSUM_MISMATCH);
        }
        let mut stored = [0u8; 8];
        stored.copy_from_slice(&bytes[GROUP_AT..LIVE]);
        if u64::from_be_bytes(stored) != group {
            return Err(GraphError::Corrupt(
                "a node meta chunk is stored under the key of another group",
            ));
        }
        Ok(Self { bytes, layout })
    }

    /// How many live nodes the group holds, as the chunk holds it.
    pub(crate) fn live(&self) -> u64 {
        u64::from(u16::from_be_bytes([self.bytes[LIVE], self.bytes[LIVE + 1]]))
    }

    /// The label sets the group uses, in code order.
    pub(crate) fn dictionary(&self) -> impl Iterator<Item = LabelSetId> + '_ {
        (0..self.layout.dictionary).map(|index| {
            let at = DICTIONARY + 2 + 4 * index;
            LabelSetId(u32::from_be_bytes([
                self.bytes[at],
                self.bytes[at + 1],
                self.bytes[at + 2],
                self.bytes[at + 3],
            ]))
        })
    }

    /// How many live nodes of the group carry `label`, as the chunk holds it.
    pub(crate) fn label_count(&self, label: Token<Label>) -> u64 {
        let start = self.layout.counts + 4;
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
        let (mut low, mut high) = (0, self.layout.labels);
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
}

/// A stored node meta chunk, read in place, with its head and body both checked.
#[derive(Clone, Copy, Debug)]
pub(crate) struct MetaBytes<'a> {
    head: MetaHead<'a>,
}

impl<'a> MetaBytes<'a> {
    pub(crate) fn parse(group: u64, bytes: &'a [u8]) -> Result<Self, GraphError> {
        let head = MetaHead::parse(group, bytes)?;
        if crc32fast::hash(&bytes[head.layout.body..]) != stored_checksum(bytes, BODY_CHECKSUM) {
            return Err(CHECKSUM_MISMATCH);
        }
        Ok(Self { head })
    }

    /// A chunk whose checksums were already checked, read without checking them again.
    pub(crate) fn trusted(bytes: &'a [u8]) -> Result<Self, GraphError> {
        Ok(Self {
            head: MetaHead {
                bytes,
                layout: Layout::of(bytes)?,
            },
        })
    }

    pub(crate) fn head(&self) -> MetaHead<'a> {
        self.head
    }

    fn alive_words(&self) -> [u64; WORDS] {
        let start = self.head.layout.body;
        let mut words = [0u64; WORDS];
        for (index, word) in words.iter_mut().enumerate() {
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(&self.head.bytes[start + 8 * index..start + 8 + 8 * index]);
            *word = u64::from_le_bytes(bytes);
        }
        words
    }

    /// Each live slot with the position of its label set in the dictionary, in slot order.
    pub(crate) fn live_codes(&self) -> impl Iterator<Item = (usize, usize)> + '_ {
        ones(self.alive_words()).map(|slot| (slot, self.code_at(slot)))
    }

    fn code_at(&self, slot: usize) -> usize {
        let layout = self.head.layout;
        let at = layout.body + BITMAP_BYTES + layout.width * slot;
        let bytes = self.head.bytes;
        match layout.width {
            0 => 0,
            1 => usize::from(bytes[at]),
            _ => usize::from(u16::from_be_bytes([bytes[at], bytes[at + 1]])),
        }
    }

    /// The position in the dictionary of the label set of the node in `slot`, if it is live.
    pub(crate) fn code(&self, slot: usize) -> Option<usize> {
        let alive = self.head.bytes[self.head.layout.body + slot / 8] & (1 << (slot % 8)) != 0;
        alive.then(|| self.code_at(slot))
    }

    pub(crate) fn label_set(&self, slot: usize) -> Result<Slot, GraphError> {
        let Some(code) = self.code(slot) else {
            return Ok(None);
        };
        self.head
            .dictionary()
            .nth(code)
            .map(Some)
            .ok_or(CODE_OUTSIDE)
    }

    pub(crate) fn to_edit(self) -> Result<MetaEdit, GraphError> {
        let dictionary: Vec<LabelSetId> = self.head.dictionary().collect();
        let mut edit = MetaEdit::default();
        for (slot, code) in self.live_codes() {
            edit.put(slot, *dictionary.get(code).ok_or(CODE_OUTSIDE)?);
        }
        Ok(edit)
    }
}

/// The node meta of one group in a write transaction: the stored chunk, the slots the stored
/// deltas and this transaction changed, and what commit needs to store the group as a rewritten
/// chunk or as deltas beside the stored one. The whole group is decoded only when a read needs
/// every slot or commit rewrites the chunk.
#[derive(Clone, Debug)]
pub(crate) struct GroupEdit {
    chunk: Option<Box<[u8]>>,
    live: u64,
    stored_deltas: BTreeMap<usize, Slot>,
    current: BTreeMap<usize, Slot>,
    touched: BTreeMap<usize, Slot>,
    fresh: usize,
    rewrite: bool,
    full: Option<MetaEdit>,
    view: OnceCell<MetaEdit>,
}

/// What commit writes for one edited group.
#[derive(Debug)]
pub(crate) enum Flush {
    /// Store the group as this chunk, or delete its chunk when no slot is live, and delete
    /// the deltas at `stale`.
    Rewrite {
        view: Box<MetaEdit>,
        stale: Vec<usize>,
    },
    /// Keep the chunk, store the deltas `put`, and delete the deltas at `delete`.
    Deltas { put: Vec<Delta>, delete: Vec<usize> },
}

fn decode(chunk: Option<&[u8]>, current: &BTreeMap<usize, Slot>) -> Result<MetaEdit, GraphError> {
    let mut edit = match chunk {
        Some(chunk) => MetaBytes::trusted(chunk)?.to_edit()?,
        None => MetaEdit::default(),
    };
    for (slot, state) in current {
        edit.set(*slot, *state);
    }
    Ok(edit)
}

fn counted(live: u64, was: Slot, is: Slot) -> Result<u64, GraphError> {
    (live + u64::from(is.is_some()))
        .checked_sub(u64::from(was.is_some()))
        .ok_or(DELTA_DISAGREES)
}

impl GroupEdit {
    /// The group as stored: its chunk, whose checksums were checked, if any, and its deltas,
    /// which must amend the chunk.
    pub(crate) fn load(chunk: Option<Box<[u8]>>, deltas: Vec<Delta>) -> Result<Self, GraphError> {
        if chunk.is_none() && !deltas.is_empty() {
            return Err(DELTA_WITHOUT_CHUNK);
        }
        let mut edit = Self {
            live: 0,
            rewrite: chunk.is_none(),
            chunk,
            stored_deltas: BTreeMap::new(),
            current: BTreeMap::new(),
            touched: BTreeMap::new(),
            fresh: 0,
            full: None,
            view: OnceCell::new(),
        };
        if let Some(chunk) = &edit.chunk {
            edit.live = MetaBytes::trusted(chunk)?.head().live();
        }
        for delta in deltas {
            if edit.chunk_state(delta.slot)? != delta.before {
                return Err(DELTA_DISAGREES);
            }
            edit.live = counted(edit.live, delta.before, delta.after)?;
            edit.stored_deltas.insert(delta.slot, delta.before);
            edit.current.insert(delta.slot, delta.after);
        }
        Ok(edit)
    }

    fn chunk_state(&self, slot: usize) -> Result<Slot, GraphError> {
        match &self.chunk {
            Some(chunk) => MetaBytes::trusted(chunk)?.label_set(slot),
            None => Ok(None),
        }
    }

    /// The label set of the node in `slot` as the transaction sees it, if the slot is live.
    pub(crate) fn label_set(&self, slot: usize) -> Result<Slot, GraphError> {
        if let Some(full) = &self.full {
            return Ok(full.label_set(slot));
        }
        match self.current.get(&slot) {
            Some(state) => Ok(*state),
            None => self.chunk_state(slot),
        }
    }

    fn build(&self) -> Result<MetaEdit, GraphError> {
        match &self.full {
            Some(full) => Ok(full.clone()),
            None => decode(self.chunk.as_deref(), &self.current),
        }
    }

    /// Every slot of the group as the transaction sees it.
    pub(crate) fn view(&self) -> Result<&MetaEdit, GraphError> {
        if let Some(full) = &self.full {
            return Ok(full);
        }
        if let Some(view) = self.view.get() {
            return Ok(view);
        }
        let view = self.build()?;
        Ok(self.view.get_or_init(|| view))
    }

    /// Sets the slot to `state`, and rewrites the chunk at commit once the group would keep
    /// more than `MAX_DELTAS` deltas.
    pub(crate) fn set(&mut self, slot: usize, state: Slot) -> Result<(), GraphError> {
        let now = self.label_set(slot)?;
        if !self.rewrite && !self.touched.contains_key(&slot) {
            let before = match self.stored_deltas.get(&slot) {
                Some(before) => *before,
                None => {
                    self.fresh += 1;
                    now
                }
            };
            self.touched.insert(slot, before);
            self.rewrite = self.stored_deltas.len() + self.fresh > MAX_DELTAS;
        }
        self.live = counted(self.live, now, state)?;
        if self.rewrite {
            if self.full.is_none() {
                self.full = Some(self.build()?);
                self.current.clear();
                self.view = OnceCell::new();
            }
            if let Some(full) = &mut self.full {
                full.set(slot, state);
            }
            return Ok(());
        }
        self.current.insert(slot, state);
        self.view = OnceCell::new();
        Ok(())
    }

    pub(crate) fn flush(self) -> Result<Flush, GraphError> {
        if self.rewrite || self.chunk.is_none() || self.live == 0 {
            let view = match (self.full, self.view.into_inner()) {
                (Some(full), _) => full,
                (None, Some(view)) => view,
                (None, None) => decode(self.chunk.as_deref(), &self.current)?,
            };
            return Ok(Flush::Rewrite {
                view: Box::new(view),
                stale: self.stored_deltas.into_keys().collect(),
            });
        }
        let mut put = Vec::new();
        let mut delete = Vec::new();
        for (slot, before) in &self.touched {
            let after = self.label_set(*slot)?;
            if after != *before {
                put.push(Delta {
                    slot: *slot,
                    before: *before,
                    after,
                });
            } else if self.stored_deltas.contains_key(slot) {
                delete.push(*slot);
            }
        }
        Ok(Flush::Deltas { put, delete })
    }
}

/// One entry of node meta as a read streams it, in key order: a group's stored chunk is
/// followed by its deltas, and a group the write transaction edited comes whole instead.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Meta<'a> {
    Chunk { group: u64, bytes: &'a [u8] },
    Delta { group: u64, delta: Delta },
    Edited { group: u64, edit: &'a MetaEdit },
}

#[derive(Debug)]
struct CachedMeta {
    bytes: Box<[u8]>,
    sets: Vec<Arc<[Token<Label>]>>,
    deltas: Vec<SlotLabels>,
}

/// The node meta chunks one query read so far, so testing a node's labels reads storage once
/// per group.
#[derive(Debug, Default)]
pub(crate) struct LabelReader {
    groups: Vec<(u64, Option<CachedMeta>)>,
    last: usize,
}

/// A stored node meta chunk, checked, with the labels of each set in its dictionary, and the
/// slots its deltas change with the labels each now holds.
pub(crate) type Loaded = (Box<[u8]>, Vec<Arc<[Token<Label>]>>, Vec<SlotLabels>);

/// A slot a delta changes, with the labels of its node now, or `None` when it holds no node.
pub(crate) type SlotLabels = (usize, Option<Arc<[Token<Label>]>>);

/// The stored chunk of a group and the deltas beside it.
pub(crate) type StoredGroup = (Box<[u8]>, Vec<Delta>);

impl LabelReader {
    /// Calls `read` with the labels of the node in `slot` of `group`, if it is live, loading
    /// the group with `load` on a miss.
    pub(crate) fn with_labels<R>(
        &mut self,
        group: u64,
        slot: usize,
        load: impl FnOnce() -> Result<Option<Loaded>, GraphError>,
        read: impl FnOnce(&[Token<Label>]) -> R,
    ) -> Result<Option<R>, GraphError> {
        let index = match self.groups.get(self.last) {
            Some((held, _)) if *held == group => self.last,
            _ => match self.groups.binary_search_by_key(&group, |(held, _)| *held) {
                Ok(index) => index,
                Err(index) => {
                    let loaded = load()?.map(|(bytes, sets, deltas)| CachedMeta {
                        bytes,
                        sets,
                        deltas,
                    });
                    self.groups.insert(index, (group, loaded));
                    index
                }
            },
        };
        self.last = index;
        let Some((_, Some(cached))) = self.groups.get(index) else {
            return Ok(None);
        };
        if let Ok(found) = cached.deltas.binary_search_by_key(&slot, |(held, _)| *held) {
            return Ok(cached.deltas[found].1.as_deref().map(read));
        }
        match MetaBytes::trusted(&cached.bytes)?.code(slot) {
            None => Ok(None),
            Some(code) => cached
                .sets
                .get(code)
                .map(|labels| read(labels))
                .ok_or(CODE_OUTSIDE)
                .map(Some),
        }
    }
}

/// Adds one to the first label count of a stored chunk and seals it again, so the
/// chunk passes its checksums but disagrees with its own slots.
#[cfg(test)]
pub(crate) fn miscount_first_label(chunk: &mut [u8]) {
    if let Ok(layout) = Layout::of(chunk) {
        let at = layout.counts + 4 + 5;
        if layout.labels > 0 {
            chunk[at] = chunk[at].wrapping_add(1);
        }
        seal(chunk, layout.body);
    }
}

fn seal(chunk: &mut [u8], body_start: usize) {
    let body = crc32fast::hash(&chunk[body_start..]);
    chunk[BODY_CHECKSUM..GROUP_AT].copy_from_slice(&body.to_le_bytes());
    let head = crc32fast::hash(&chunk[BODY_CHECKSUM..body_start]);
    chunk[HEAD_CHECKSUM..BODY_CHECKSUM].copy_from_slice(&head.to_le_bytes());
}

#[cfg(test)]
#[path = "meta.test.rs"]
mod tests;
