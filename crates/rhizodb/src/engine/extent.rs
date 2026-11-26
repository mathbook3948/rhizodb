use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt;

use super::error::EngineError;
use super::header::OPEN_EXTENTS;
use super::header::OpenExtent;
use super::layout::BlockRef;
use super::layout::EXTENT_BLOCKS;
use super::layout::ExtentId;

/// What an extent was opened for. Files of engine format minor 1 and before kept an open extent
/// for each purpose, and every extent opened since is a tree extent that takes every kind of
/// block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Purpose {
    Tree,
    Values,
    Meta,
}

impl Purpose {
    pub(crate) const ALL: [Purpose; 3] = [Purpose::Tree, Purpose::Values, Purpose::Meta];

    pub(crate) fn index(self) -> usize {
        match self {
            Purpose::Tree => 0,
            Purpose::Values => 1,
            Purpose::Meta => 2,
        }
    }

    pub(crate) fn code(self) -> u8 {
        self.index() as u8
    }

    pub(crate) fn from_code(code: u8) -> Option<Self> {
        Purpose::ALL
            .into_iter()
            .find(|purpose| purpose.code() == code)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExtentState {
    /// A checkpoint may append to it.
    Open,
    /// Full, and never written again.
    Sealed,
}

const WORDS: usize = EXTENT_BLOCKS as usize / 64;

/// A set of the blocks of one extent.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Blocks([u64; WORDS]);

impl Blocks {
    /// The `count` blocks from `start`, cut at the end of the extent.
    pub(crate) fn range(start: u32, count: u32) -> Self {
        let mut words = [0; WORDS];
        let end = start.saturating_add(count).min(EXTENT_BLOCKS);
        for block in start..end {
            words[block as usize / 64] |= 1 << (block % 64);
        }
        Self(words)
    }

    pub(crate) fn count(&self) -> u32 {
        self.0.iter().map(|word| word.count_ones()).sum()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.iter().all(|word| *word == 0)
    }

    pub(crate) fn contains_all(&self, other: &Blocks) -> bool {
        self.0
            .iter()
            .zip(other.0)
            .all(|(mine, theirs)| theirs & !mine == 0)
    }

    pub(crate) fn intersects(&self, other: &Blocks) -> bool {
        self.0
            .iter()
            .zip(other.0)
            .any(|(mine, theirs)| mine & theirs != 0)
    }

    pub(crate) fn with(&self, other: &Blocks) -> Blocks {
        let mut words = self.0;
        for (word, theirs) in words.iter_mut().zip(other.0) {
            *word |= theirs;
        }
        Blocks(words)
    }

    pub(crate) fn without(&self, other: &Blocks) -> Blocks {
        let mut words = self.0;
        for (word, theirs) in words.iter_mut().zip(other.0) {
            *word &= !theirs;
        }
        Blocks(words)
    }

    fn next_from(&self, from: u32, present: bool) -> Option<u32> {
        let mut index = from as usize / 64;
        let mut mask = u64::MAX.checked_shl(from % 64).unwrap_or(0);
        while index < WORDS {
            let word = if present {
                self.0[index]
            } else {
                !self.0[index]
            } & mask;
            if word != 0 {
                return Some(index as u32 * 64 + word.trailing_zeros());
            }
            index += 1;
            mask = u64::MAX;
        }
        None
    }

    /// The runs of blocks in a row the set holds, as starts and lengths, lowest first.
    pub(crate) fn ranges(&self) -> impl Iterator<Item = (u32, u32)> + '_ {
        let mut from = 0;
        std::iter::from_fn(move || {
            let start = self.next_from(from, true)?;
            let end = self.next_from(start, false).unwrap_or(EXTENT_BLOCKS);
            from = end;
            Some((start, end - start))
        })
    }

    /// The first block of the lowest run of at least `count` blocks in a row.
    pub(crate) fn first_run(&self, count: u32) -> Option<u32> {
        self.ranges()
            .find(|(_, length)| *length >= count)
            .map(|(start, _)| start)
    }
}

impl fmt::Debug for Blocks {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_list()
            .entries(self.ranges().map(|(start, length)| start..start + length))
            .finish()
    }
}

/// One extent of the extent table. The opened term and the replication scheme are kept for
/// replicated extents, and are zero locally.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ExtentEntry {
    pub(crate) slot: u64,
    pub(crate) written: u32,
    /// The blocks a page or run of the checkpoint's tree occupies, all below `written`.
    pub(crate) used: Blocks,
    pub(crate) state: ExtentState,
    pub(crate) purpose: Purpose,
    pub(crate) scheme: u8,
    pub(crate) opened_term: u64,
}

impl ExtentEntry {
    /// The blocks a page or run occupies.
    pub(crate) fn live(&self) -> u32 {
        self.used.count()
    }

    /// The blocks below the written length that no page or run occupies.
    pub(crate) fn free(&self) -> Blocks {
        Blocks::range(0, self.written).without(&self.used)
    }
}

/// Every extent of the file, the counters that name the next ones, and the open extent of
/// each purpose.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExtentTable {
    pub(crate) entries: BTreeMap<ExtentId, ExtentEntry>,
    pub(crate) next_extent: ExtentId,
    pub(crate) next_slot: u64,
    pub(crate) open: [Option<ExtentId>; OPEN_EXTENTS],
    /// Free slots a checkpoint may place new extents in, lowest first. The engine fills it at the
    /// start of each checkpoint, and it is never stored.
    pub(crate) reusable: BTreeSet<u64>,
    /// Free blocks of extents in the table that a checkpoint may place pages and runs in, by
    /// extent in slot order. The engine fills it at the start of each checkpoint, and it is
    /// never stored.
    pub(crate) holes: Vec<(ExtentId, Blocks)>,
}

impl Default for ExtentTable {
    fn default() -> Self {
        Self {
            entries: BTreeMap::new(),
            next_extent: ExtentId(1),
            next_slot: 0,
            open: [None; OPEN_EXTENTS],
            reusable: BTreeSet::new(),
            holes: Vec::new(),
        }
    }
}

impl ExtentTable {
    /// Reserves `blocks` blocks in a row in the lowest hole they fit in, or else as
    /// [`Self::append`] does.
    pub(crate) fn place(
        &mut self,
        blocks: u32,
        opened: impl FnMut(ExtentId, u64),
    ) -> Result<BlockRef, EngineError> {
        self.place_where(blocks, false, opened)
    }

    /// [`Self::place`] in a hole of an open extent only, so a header that names the open extents
    /// can find what it holds.
    pub(crate) fn place_in_open(
        &mut self,
        blocks: u32,
        opened: impl FnMut(ExtentId, u64),
    ) -> Result<BlockRef, EngineError> {
        self.place_where(blocks, true, opened)
    }

    fn place_where(
        &mut self,
        blocks: u32,
        open_only: bool,
        opened: impl FnMut(ExtentId, u64),
    ) -> Result<BlockRef, EngineError> {
        if blocks == 0 || blocks > EXTENT_BLOCKS {
            return Err(EngineError::TooLarge);
        }
        self.holes.retain(|(_, free)| !free.is_empty());
        let open = self.open;
        let found = self.holes.iter_mut().find_map(|(id, free)| {
            if open_only && !open.contains(&Some(*id)) {
                return None;
            }
            let start = free.first_run(blocks)?;
            let taken = Blocks::range(start, blocks);
            *free = free.without(&taken);
            Some((*id, start, taken))
        });
        let Some((id, start, taken)) = found else {
            return self.append(blocks, opened);
        };
        let entry = self.entry_mut(id)?;
        if entry.used.intersects(&taken) || start + blocks > entry.written {
            return Err(EngineError::corrupt(format!(
                "{blocks} blocks at block {start} of extent {} were placed in a hole that is not free",
                id.0
            )));
        }
        entry.used = entry.used.with(&taken);
        BlockRef::new(id, start)
    }

    /// Reserves `blocks` blocks in a row at the end of the first open extent they fit in. When
    /// they fit in none, it seals every open extent and opens a new one. Calls `opened` with each
    /// new extent and its slot.
    pub(crate) fn append(
        &mut self,
        blocks: u32,
        mut opened: impl FnMut(ExtentId, u64),
    ) -> Result<BlockRef, EngineError> {
        if blocks == 0 || blocks > EXTENT_BLOCKS {
            return Err(EngineError::TooLarge);
        }
        for id in self.open.into_iter().flatten() {
            let entry = self.entry_mut(id)?;
            if entry.written + blocks <= EXTENT_BLOCKS {
                let at = BlockRef::new(id, entry.written)?;
                entry.used = entry.used.with(&Blocks::range(entry.written, blocks));
                entry.written += blocks;
                return Ok(at);
            }
        }
        for id in self.open.into_iter().flatten() {
            self.entry_mut(id)?.state = ExtentState::Sealed;
        }
        self.open = [None; OPEN_EXTENTS];
        let id = self.next_extent;
        self.next_extent = ExtentId(id.0 + 1);
        let slot = match self.reusable.pop_first() {
            Some(slot) => slot,
            None => {
                self.next_slot += 1;
                self.next_slot - 1
            }
        };
        self.entries.insert(
            id,
            ExtentEntry {
                slot,
                written: blocks,
                used: Blocks::range(0, blocks),
                state: ExtentState::Open,
                purpose: Purpose::Tree,
                scheme: 0,
                opened_term: 0,
            },
        );
        self.open[0] = Some(id);
        opened(id, slot);
        BlockRef::new(id, 0)
    }

    /// Seals the open extents when a free slot lies below the lowest of them, so the next block
    /// appended opens an extent there and the slots above can empty and be cut off the file.
    pub(crate) fn move_down(&mut self) {
        let lowest_open = self
            .open
            .into_iter()
            .flatten()
            .filter_map(|id| self.entries.get(&id).map(|entry| entry.slot))
            .min();
        let lowest_free = self.reusable.first().copied();
        if let (Some(open), Some(free)) = (lowest_open, lowest_free)
            && free < open
        {
            for id in self.open.into_iter().flatten() {
                if let Some(entry) = self.entries.get_mut(&id) {
                    entry.state = ExtentState::Sealed;
                }
            }
            self.open = [None; OPEN_EXTENTS];
        }
    }

    /// The slots below the high-water mark that no extent uses.
    pub(crate) fn free_slots(&self) -> BTreeSet<u64> {
        let used: BTreeSet<u64> = self.entries.values().map(|entry| entry.slot).collect();
        (0..self.next_slot)
            .filter(|slot| !used.contains(slot))
            .collect()
    }

    /// Removes every sealed extent with no live block and returns their slots.
    pub(crate) fn drop_empty(&mut self) -> Vec<u64> {
        let empty: Vec<ExtentId> = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.state == ExtentState::Sealed && entry.used.is_empty())
            .map(|(id, _)| *id)
            .collect();
        empty
            .into_iter()
            .filter_map(|id| self.entries.remove(&id).map(|entry| entry.slot))
            .collect()
    }

    /// Seals every extent, counts none of its blocks live, and gives up its holes, before a
    /// rewrite of every table moves all live data into new extents.
    pub(crate) fn evacuate(&mut self) {
        for entry in self.entries.values_mut() {
            entry.state = ExtentState::Sealed;
            entry.used = Blocks::default();
        }
        self.open = [None; OPEN_EXTENTS];
        self.holes.clear();
    }

    /// Lowers the high-water mark past free slots at the end of the file, and returns the slot
    /// and written blocks of the last extent in use, after which the file can end.
    pub(crate) fn trim(&mut self, removable: &BTreeSet<u64>) -> Option<(u64, u32)> {
        let last = self.entries.values().max_by_key(|entry| entry.slot);
        let top = last.map_or(0, |entry| entry.slot + 1);
        if (top..self.next_slot).any(|slot| !removable.contains(&slot)) {
            return None;
        }
        let end = last.map_or((0, 0), |entry| (entry.slot, entry.written));
        self.next_slot = top;
        self.reusable.retain(|slot| *slot < top);
        Some(end)
    }

    /// Records that `blocks` blocks from `at` are no longer reachable.
    pub(crate) fn free(&mut self, at: BlockRef, blocks: u32) -> Result<(), EngineError> {
        let entry = self.entry_mut(at.extent())?;
        let freed = Blocks::range(at.block(), blocks);
        if at
            .block()
            .checked_add(blocks)
            .is_none_or(|end| end > entry.written)
            || !entry.used.contains_all(&freed)
        {
            return Err(EngineError::corrupt(format!(
                "{blocks} blocks at {at:?} were freed but are not live"
            )));
        }
        entry.used = entry.used.without(&freed);
        Ok(())
    }

    fn entry_mut(&mut self, id: ExtentId) -> Result<&mut ExtentEntry, EngineError> {
        self.entries
            .get_mut(&id)
            .ok_or_else(|| EngineError::corrupt(format!("extent {} is not in the table", id.0)))
    }

    /// The header's record of the open extents.
    pub(crate) fn open_extents(&self) -> Result<[OpenExtent; OPEN_EXTENTS], EngineError> {
        let mut open = [OpenExtent::default(); OPEN_EXTENTS];
        for (entry, id) in open.iter_mut().zip(self.open) {
            if let Some(id) = id {
                let extent = self
                    .entries
                    .get(&id)
                    .ok_or_else(|| EngineError::corrupt("an open extent is not in the table"))?;
                *entry = OpenExtent {
                    id,
                    slot: extent.slot,
                    written: extent.written,
                };
            }
        }
        Ok(open)
    }

    /// Fails unless the header's open extents are this table's.
    pub(crate) fn check_open(
        &self,
        header: &[OpenExtent; OPEN_EXTENTS],
    ) -> Result<(), EngineError> {
        if &self.open_extents()? != header {
            return Err(EngineError::corrupt(
                "the header's open extents differ from the catalog's",
            ));
        }
        let mut slots = std::collections::BTreeSet::new();
        for (id, entry) in &self.entries {
            if *id >= self.next_extent
                || entry.slot >= self.next_slot
                || !slots.insert(entry.slot)
                || !Blocks::range(0, entry.written).contains_all(&entry.used)
                || entry.written > EXTENT_BLOCKS
                || (entry.state == ExtentState::Open) != self.open.contains(&Some(*id))
                || self
                    .open
                    .get(entry.purpose.index())
                    .is_some_and(|open| *open == Some(*id) && entry.state != ExtentState::Open)
            {
                return Err(EngineError::corrupt(format!(
                    "extent {} has an invalid table entry",
                    id.0
                )));
            }
        }
        for (index, id) in self.open.iter().enumerate() {
            if let Some(id) = id
                && self
                    .entries
                    .get(id)
                    .is_none_or(|entry| entry.purpose.index() != index)
            {
                return Err(EngineError::corrupt("an open extent has another purpose"));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "extent.test.rs"]
mod tests;
