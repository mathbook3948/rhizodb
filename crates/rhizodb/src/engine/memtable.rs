use std::collections::BTreeMap;
use std::ops::Bound;
use std::sync::Arc;
use std::sync::PoisonError;
use std::sync::RwLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use super::layout::TableId;

/// Bytes shared between the memtable, transactions, and readers without copying.
pub(crate) type Bytes = Arc<[u8]>;

/// A value, or a tombstone that hides every older value of its key.
pub(crate) type Slot = Option<Bytes>;

/// What one layer of the read path says about a key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Lookup {
    Found(Bytes),
    Deleted,
    /// The layer knows nothing of the key, so an older layer decides.
    Absent,
}

#[derive(Clone, Debug)]
struct Version {
    lsn: u64,
    slot: Slot,
}

type Chains = BTreeMap<TableId, BTreeMap<Bytes, Vec<Version>>>;

const VERSION_OVERHEAD: u64 = 64;

/// The committed writes not yet in the database file: per table, each key's versions in LSN
/// order. A reader at snapshot S sees each key's newest version at or below S.
#[derive(Debug, Default)]
pub(crate) struct Memtable {
    chains: RwLock<Chains>,
    bytes: AtomicU64,
}

fn cost(key: &[u8], slot: &Slot) -> u64 {
    (key.len() + slot.as_ref().map_or(0, |value| value.len())) as u64 + VERSION_OVERHEAD
}

fn visible(chain: &[Version], snapshot: u64) -> Option<&Version> {
    chain.iter().rev().find(|version| version.lsn <= snapshot)
}

impl Memtable {
    fn read(&self) -> std::sync::RwLockReadGuard<'_, Chains> {
        self.chains.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// The approximate memory the versions hold, which drives checkpoints.
    pub(crate) fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.read().values().all(BTreeMap::is_empty)
    }

    pub(crate) fn get(&self, table: TableId, key: &[u8], snapshot: u64) -> Lookup {
        let chains = self.read();
        let Some(version) = chains
            .get(&table)
            .and_then(|keys| keys.get(key))
            .and_then(|chain| visible(chain, snapshot))
        else {
            return Lookup::Absent;
        };
        match &version.slot {
            Some(value) => Lookup::Found(Arc::clone(value)),
            None => Lookup::Deleted,
        }
    }

    /// Every key between the bounds that has a version visible at `snapshot`, tombstones
    /// included, in key order.
    pub(crate) fn range(
        &self,
        table: TableId,
        start: Bound<&[u8]>,
        end: Bound<&[u8]>,
        snapshot: u64,
    ) -> Vec<(Bytes, Slot)> {
        if !bounds_are_ordered(start, end) {
            return Vec::new();
        }
        let chains = self.read();
        let Some(keys) = chains.get(&table) else {
            return Vec::new();
        };
        keys.range::<[u8], _>((start, end))
            .filter_map(|(key, chain)| {
                visible(chain, snapshot).map(|version| (Arc::clone(key), version.slot.clone()))
            })
            .collect()
    }

    /// Adds the versions of the commit at `lsn`, and drops every version that no snapshot at or
    /// after `horizon` can see.
    pub(crate) fn apply(
        &self,
        lsn: u64,
        changes: impl IntoIterator<Item = (TableId, Bytes, Slot)>,
        horizon: u64,
    ) {
        let mut chains = self.chains.write().unwrap_or_else(PoisonError::into_inner);
        let mut added = 0u64;
        let mut freed = 0u64;
        for (table, key, slot) in changes {
            let key_length = key.len() as u64;
            added += cost(&key, &slot);
            let chain = chains.entry(table).or_default().entry(key).or_default();
            chain.push(Version { lsn, slot });
            if let Some(newest_hidden) = chain.iter().rposition(|version| version.lsn <= horizon) {
                for version in chain.drain(..newest_hidden) {
                    freed += key_length + cost(&[], &version.slot);
                }
            }
        }
        drop(chains);
        self.bytes.fetch_add(added, Ordering::Relaxed);
        self.bytes
            .fetch_sub(freed.min(self.bytes()), Ordering::Relaxed);
    }

    /// Every key's newest version, per table in key order, which a checkpoint writes to the file.
    pub(crate) fn newest(&self) -> BTreeMap<TableId, Vec<(Bytes, Slot)>> {
        self.read()
            .iter()
            .map(|(table, keys)| {
                let newest = keys
                    .iter()
                    .filter_map(|(key, chain)| {
                        chain
                            .last()
                            .map(|version| (Arc::clone(key), version.slot.clone()))
                    })
                    .collect();
                (*table, newest)
            })
            .collect()
    }
}

/// Whether a range from `start` to `end` can hold any key. A `BTreeMap` panics on a reversed one.
pub(crate) fn bounds_are_ordered(start: Bound<&[u8]>, end: Bound<&[u8]>) -> bool {
    match (start, end) {
        (Bound::Included(start), Bound::Excluded(end))
        | (Bound::Excluded(start), Bound::Excluded(end)) => start < end,
        (Bound::Included(start), Bound::Included(end)) => start <= end,
        (Bound::Excluded(start), Bound::Included(end)) => start < end,
        _ => true,
    }
}

#[cfg(test)]
#[path = "memtable.test.rs"]
mod tests;
