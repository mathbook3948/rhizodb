use std::borrow::Borrow;
use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::ops::Bound;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::RwLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use twox_hash::XxHash3_64;

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

type Chains = BTreeMap<TableId, BTreeMap<Key, Vec<Version>>>;

trait Ranked {
    fn head(&self) -> u128;

    fn bytes(&self) -> &[u8];
}

fn head_of(bytes: &[u8]) -> u128 {
    match bytes.first_chunk::<16>() {
        Some(head) => u128::from_be_bytes(*head),
        None => bytes.iter().enumerate().fold(0, |head, (at, byte)| {
            head | u128::from(*byte) << (120 - 8 * at)
        }),
    }
}

fn rank(left: &dyn Ranked, right: &dyn Ranked) -> std::cmp::Ordering {
    let (left_bytes, right_bytes) = (left.bytes(), right.bytes());
    match left.head().cmp(&right.head()) {
        std::cmp::Ordering::Equal if left_bytes.len() <= 16 || right_bytes.len() <= 16 => {
            left_bytes.len().cmp(&right_bytes.len())
        }
        std::cmp::Ordering::Equal => left_bytes[16..].cmp(&right_bytes[16..]),
        order => order,
    }
}

impl PartialEq for dyn Ranked + '_ {
    fn eq(&self, other: &Self) -> bool {
        rank(self, other).is_eq()
    }
}

impl Eq for dyn Ranked + '_ {}

impl PartialOrd for dyn Ranked + '_ {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for dyn Ranked + '_ {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        rank(self, other)
    }
}

#[derive(Clone, Debug)]
struct Key {
    head: u128,
    bytes: Bytes,
}

impl Key {
    fn new(bytes: Bytes) -> Self {
        Self {
            head: head_of(&bytes),
            bytes,
        }
    }
}

impl Ranked for Key {
    fn head(&self) -> u128 {
        self.head
    }

    fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl PartialEq for Key {
    fn eq(&self, other: &Self) -> bool {
        rank(self, other).is_eq()
    }
}

impl Eq for Key {}

impl PartialOrd for Key {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Key {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        rank(self, other)
    }
}

impl<'a> Borrow<dyn Ranked + 'a> for Key {
    fn borrow(&self) -> &(dyn Ranked + 'a) {
        self
    }
}

struct Probe<'a> {
    head: u128,
    bytes: &'a [u8],
}

impl<'a> Probe<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            head: head_of(bytes),
            bytes,
        }
    }
}

impl Ranked for Probe<'_> {
    fn head(&self) -> u128 {
        self.head
    }

    fn bytes(&self) -> &[u8] {
        self.bytes
    }
}

fn probe_bound<'a>(bound: &'a Bound<Probe<'a>>) -> Bound<&'a dyn Ranked> {
    match bound {
        Bound::Included(probe) => Bound::Included(probe),
        Bound::Excluded(probe) => Bound::Excluded(probe),
        Bound::Unbounded => Bound::Unbounded,
    }
}

const VERSION_OVERHEAD: u64 = 64;

const FILTER_BLOCKS: usize = 1 << 14;

const FILTER_PROBES: u32 = 4;

#[derive(Debug)]
struct Filter {
    words: Box<[AtomicU64]>,
}

impl Default for Filter {
    fn default() -> Self {
        Self {
            words: (0..FILTER_BLOCKS * 8).map(|_| AtomicU64::new(0)).collect(),
        }
    }
}

fn filter_hash(table: TableId, key: &[u8]) -> u64 {
    XxHash3_64::oneshot_with_seed(u64::from(table.0), key)
}

impl Filter {
    fn bits(hash: u64) -> impl Iterator<Item = (usize, u64)> {
        let block = (hash >> 40) as usize % FILTER_BLOCKS * 8;
        (0..FILTER_PROBES).map(move |probe| {
            let bit = (hash >> (9 * probe)) & 511;
            (block + (bit / 64) as usize, 1 << (bit % 64))
        })
    }

    fn insert_under_write_lock(&self, hash: u64) {
        for (word, mask) in Self::bits(hash) {
            let word = &self.words[word];
            let bits = word.load(Ordering::Relaxed);
            if bits & mask == 0 {
                word.store(bits | mask, Ordering::Relaxed);
            }
        }
    }

    fn may_hold(&self, hash: u64) -> bool {
        Self::bits(hash).all(|(word, mask)| self.words[word].load(Ordering::Relaxed) & mask != 0)
    }
}

/// The committed writes not yet in the database file: per table, each key's versions in LSN
/// order. A reader at snapshot S sees each key's newest version at or below S.
#[derive(Debug, Default)]
pub(crate) struct Memtable {
    chains: RwLock<Chains>,
    filter: Filter,
    bytes: AtomicU64,
    graves: Mutex<VecDeque<(TableId, Key, u64)>>,
    /// How many entries ranges have copied out, for tests that bound what a scan reads.
    #[cfg(test)]
    pub(crate) copied: AtomicU64,
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

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.read().values().all(BTreeMap::is_empty)
    }

    pub(crate) fn get(&self, table: TableId, key: &[u8], snapshot: u64) -> Lookup {
        if self.bytes() == 0 || !self.filter.may_hold(filter_hash(table, key)) {
            return Lookup::Absent;
        }
        let chains = self.read();
        let Some(version) = chains
            .get(&table)
            .and_then(|keys| keys.get(&Probe::new(key) as &dyn Ranked))
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
    /// included, in key order. It copies the keys out in batches that grow as it goes, so a
    /// reader that stops early copies about as many as it read, not the whole range.
    pub(crate) fn range<'a>(
        &'a self,
        table: TableId,
        start: Bound<&'a [u8]>,
        end: Bound<&'a [u8]>,
        snapshot: u64,
    ) -> Range<'a> {
        let batch = if self.bytes() == 0 { 0 } else { FIRST_BATCH };
        Range {
            memtable: self,
            table,
            start,
            end,
            snapshot,
            after: None,
            entries: Vec::new().into_iter(),
            next_batch: batch,
        }
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
        let mut freed = self.sweep(&mut chains, horizon);
        for (table, key, slot) in changes {
            let key_length = key.len() as u64;
            self.filter
                .insert_under_write_lock(filter_hash(table, &key));
            added += cost(&key, &slot);
            let chain = chains
                .entry(table)
                .or_default()
                .entry(Key::new(key))
                .or_default();
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

    /// Records that no layer below this memtable holds any of `keys`, which the commit at `lsn`
    /// deleted, so once every snapshot sees that commit their chains can go instead of hiding
    /// nothing until a checkpoint.
    pub(crate) fn bury(&self, lsn: u64, keys: impl IntoIterator<Item = (TableId, Bytes)>) {
        let mut graves = self.graves.lock().unwrap_or_else(PoisonError::into_inner);
        graves.extend(
            keys.into_iter()
                .map(|(table, key)| (table, Key::new(key), lsn)),
        );
    }

    fn sweep(&self, chains: &mut Chains, horizon: u64) -> u64 {
        let mut graves = self.graves.lock().unwrap_or_else(PoisonError::into_inner);
        let mut freed = 0u64;
        while let Some((table, key, lsn)) = graves.pop_front_if(|(_, _, lsn)| *lsn <= horizon) {
            let Some(keys) = chains.get_mut(&table) else {
                continue;
            };
            let deleted_last = keys
                .get(&key)
                .and_then(|chain| chain.last())
                .is_some_and(|version| version.lsn == lsn && version.slot.is_none());
            if deleted_last && let Some(chain) = keys.remove(&key) {
                let key_length = key.bytes.len() as u64;
                freed += chain
                    .iter()
                    .map(|version| key_length + cost(&[], &version.slot))
                    .sum::<u64>();
            }
        }
        freed
    }

    /// A memtable that holds each key's newest version if a commit after `lsn` wrote it, for
    /// readers that read a tree holding every commit up to `lsn`.
    pub(crate) fn after(&self, lsn: u64) -> Memtable {
        let kept = Memtable::default();
        let mut added = 0u64;
        {
            let mut into = kept.chains.write().unwrap_or_else(PoisonError::into_inner);
            for (table, keys) in self.read().iter() {
                for (key, chain) in keys {
                    let Some(version) = chain.last().filter(|version| version.lsn > lsn) else {
                        continue;
                    };
                    kept.filter
                        .insert_under_write_lock(filter_hash(*table, &key.bytes));
                    added += cost(&key.bytes, &version.slot);
                    into.entry(*table).or_default().insert(
                        Key::new(Arc::clone(&key.bytes)),
                        vec![Version {
                            lsn: version.lsn,
                            slot: version.slot.clone(),
                        }],
                    );
                }
            }
        }
        kept.bytes.store(added, Ordering::Relaxed);
        kept
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
                            .map(|version| (Arc::clone(&key.bytes), version.slot.clone()))
                    })
                    .collect();
                (*table, newest)
            })
            .collect()
    }
}

const FIRST_BATCH: usize = 16;

const LAST_BATCH: usize = 1024;

/// The entries of [`Memtable::range`]. Each batch takes the read lock on its own, so no lock is
/// held while the reader handles an entry, and it resumes after the last key it copied. That
/// key's visible version is still there, since a commit drops a version only once no open snapshot
/// needs it.
#[derive(Debug)]
pub(crate) struct Range<'a> {
    memtable: &'a Memtable,
    table: TableId,
    start: Bound<&'a [u8]>,
    end: Bound<&'a [u8]>,
    snapshot: u64,
    after: Option<Bytes>,
    entries: std::vec::IntoIter<(Bytes, Slot)>,
    next_batch: usize,
}

impl Range<'_> {
    fn refill(&mut self) {
        let start = match &self.after {
            Some(after) => Bound::Excluded(&**after),
            None => self.start,
        };
        let mut entries = Vec::new();
        if bounds_are_ordered(start, self.end) {
            let chains = self.memtable.read();
            if let Some(keys) = chains.get(&self.table) {
                let (start, end) = (start.map(Probe::new), self.end.map(Probe::new));
                entries.extend(
                    keys.range::<dyn Ranked, _>((probe_bound(&start), probe_bound(&end)))
                        .filter_map(|(key, chain)| {
                            visible(chain, self.snapshot)
                                .map(|version| (Arc::clone(&key.bytes), version.slot.clone()))
                        })
                        .take(self.next_batch),
                );
            }
        }
        #[cfg(test)]
        self.memtable
            .copied
            .fetch_add(entries.len() as u64, Ordering::Relaxed);
        self.next_batch = if entries.len() < self.next_batch {
            0
        } else {
            (self.next_batch * 2).min(LAST_BATCH)
        };
        self.after = entries.last().map(|(key, _)| Arc::clone(key));
        self.entries = entries.into_iter();
    }
}

impl Iterator for Range<'_> {
    type Item = (Bytes, Slot);

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(entry) = self.entries.next() {
            return Some(entry);
        }
        if self.next_batch == 0 {
            return None;
        }
        self.refill();
        self.entries.next()
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
