use std::collections::HashMap;
use std::hash::BuildHasherDefault;
use std::hash::Hasher;
use std::sync::Arc;
use std::sync::PoisonError;
use std::sync::RwLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use super::layout::BlockRef;
use super::page::PageHeader;

/// A page that passed its checksum and placement checks, with its decoded header.
#[derive(Debug)]
pub(crate) struct CheckedPage {
    pub(crate) header: PageHeader,
    pub(crate) bytes: Box<[u8]>,
}

/// A checked page, which never changes once written.
pub(crate) type CachedPage = Arc<CheckedPage>;

const SHARDS: usize = 64;

#[derive(Debug)]
struct Entry {
    at: BlockRef,
    page: CachedPage,
    used: AtomicBool,
}

/// Hashes a block reference by one multiplication, since references are not chosen by an
/// adversary and the cache is on every read.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct BlockHasher(u64);

impl Hasher for BlockHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 = (self.0.rotate_left(8) ^ u64::from(*byte)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        }
    }

    fn write_u64(&mut self, value: u64) {
        self.0 = (self.0 ^ value)
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .rotate_left(29);
    }
}

#[derive(Debug, Default)]
struct Shard {
    index: HashMap<BlockRef, usize, BuildHasherDefault<BlockHasher>>,
    entries: Vec<Entry>,
    hand: usize,
}

/// A sharded CLOCK cache of checked pages keyed by block reference. Pages are immutable, so an
/// entry is never invalidated.
#[derive(Debug)]
pub(crate) struct Cache {
    shards: Vec<RwLock<Shard>>,
    per_shard: usize,
}

impl Cache {
    /// A cache holding about `pages` pages.
    pub(crate) fn new(pages: usize) -> Self {
        Self {
            shards: (0..SHARDS).map(|_| RwLock::default()).collect(),
            per_shard: pages.div_ceil(SHARDS).max(1),
        }
    }

    fn shard(&self, at: BlockRef) -> &RwLock<Shard> {
        let mixed = at.raw().wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 58;
        &self.shards[mixed as usize % SHARDS]
    }

    pub(crate) fn get(&self, at: BlockRef) -> Option<CachedPage> {
        let shard = self
            .shard(at)
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        let entry = &shard.entries[*shard.index.get(&at)?];
        if !entry.used.load(Ordering::Relaxed) {
            entry.used.store(true, Ordering::Relaxed);
        }
        Some(Arc::clone(&entry.page))
    }

    pub(crate) fn insert(&self, at: BlockRef, page: CachedPage) {
        let per_shard = self.per_shard;
        let mut shard = self
            .shard(at)
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        if shard.index.contains_key(&at) {
            return;
        }
        if shard.entries.len() < per_shard {
            let index = shard.entries.len();
            shard.entries.push(Entry {
                at,
                page,
                used: AtomicBool::new(false),
            });
            shard.index.insert(at, index);
            return;
        }
        loop {
            let hand = shard.hand;
            shard.hand = (hand + 1) % shard.entries.len();
            let entry = &mut shard.entries[hand];
            if *entry.used.get_mut() {
                *entry.used.get_mut() = false;
                continue;
            }
            let evicted = entry.at;
            *entry = Entry {
                at,
                page,
                used: AtomicBool::new(false),
            };
            shard.index.remove(&evicted);
            shard.index.insert(at, hand);
            return;
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.shards
            .iter()
            .map(|shard| {
                shard
                    .read()
                    .unwrap_or_else(PoisonError::into_inner)
                    .entries
                    .len()
            })
            .sum()
    }
}

#[cfg(test)]
#[path = "cache.test.rs"]
mod tests;
