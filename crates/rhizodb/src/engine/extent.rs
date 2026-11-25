use std::collections::BTreeMap;
use std::collections::BTreeSet;

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

/// One extent of the extent table. The opened term and the replication scheme are kept for
/// replicated extents, and are zero locally.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ExtentEntry {
    pub(crate) slot: u64,
    pub(crate) written: u32,
    pub(crate) live: u32,
    pub(crate) state: ExtentState,
    pub(crate) purpose: Purpose,
    pub(crate) scheme: u8,
    pub(crate) opened_term: u64,
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
}

impl Default for ExtentTable {
    fn default() -> Self {
        Self {
            entries: BTreeMap::new(),
            next_extent: ExtentId(1),
            next_slot: 0,
            open: [None; OPEN_EXTENTS],
            reusable: BTreeSet::new(),
        }
    }
}

impl ExtentTable {
    /// Reserves `blocks` blocks in a row in the first open extent they fit in. When they fit in
    /// none, it seals every open extent and opens a new one. Calls `opened` with each new extent
    /// and its slot.
    pub(crate) fn place(
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
                entry.written += blocks;
                entry.live += blocks;
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
                live: blocks,
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
            .filter(|(_, entry)| entry.state == ExtentState::Sealed && entry.live == 0)
            .map(|(id, _)| *id)
            .collect();
        empty
            .into_iter()
            .filter_map(|id| self.entries.remove(&id).map(|entry| entry.slot))
            .collect()
    }

    /// Seals every extent and counts none of its blocks live, before a rewrite of every table
    /// moves all live data into new extents.
    pub(crate) fn evacuate(&mut self) {
        for entry in self.entries.values_mut() {
            entry.state = ExtentState::Sealed;
            entry.live = 0;
        }
        self.open = [None; OPEN_EXTENTS];
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
        if at
            .block()
            .checked_add(blocks)
            .is_none_or(|end| end > entry.written)
            || blocks > entry.live
        {
            return Err(EngineError::corrupt(format!(
                "{blocks} blocks at {at:?} were freed but are not live"
            )));
        }
        entry.live -= blocks;
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
                || entry.live > entry.written
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
