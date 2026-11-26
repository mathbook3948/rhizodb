use std::collections::HashMap;
use std::hash::BuildHasherDefault;
use std::hash::Hasher;
use std::sync::Arc;
use std::sync::PoisonError;
use std::sync::RwLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
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
    entries: HashMap<BlockRef, Entry, BuildHasherDefault<BlockHasher>>,
    clock: Vec<BlockRef>,
    hand: usize,
    hits: AtomicU64,
    misses: AtomicU64,
}

/// A sharded CLOCK cache of checked pages keyed by block reference. A block takes a new page
/// only once no tree reaches its old one, and writing it removes the old page.
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
        let Some(entry) = shard.entries.get(&at) else {
            shard.misses.fetch_add(1, Ordering::Relaxed);
            return None;
        };
        shard.hits.fetch_add(1, Ordering::Relaxed);
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
        let shard = &mut *shard;
        if shard.entries.contains_key(&at) {
            return;
        }
        let entry = Entry {
            page,
            used: AtomicBool::new(false),
        };
        if shard.clock.len() < per_shard {
            shard.clock.push(at);
            shard.entries.insert(at, entry);
            return;
        }
        loop {
            let hand = shard.hand;
            shard.hand = (hand + 1) % shard.clock.len();
            let held = shard.clock[hand];
            if let Some(used) = shard
                .entries
                .get_mut(&held)
                .map(|entry| entry.used.get_mut())
                && *used
            {
                *used = false;
                continue;
            }
            shard.entries.remove(&held);
            shard.clock[hand] = at;
            shard.entries.insert(at, entry);
            return;
        }
    }

    /// Forgets the page held for `at`, whose block is being written again. Its place on the
    /// clock stays until the hand reaches it.
    pub(crate) fn remove(&self, at: BlockRef) {
        let shard = self.shard(at);
        if !shard
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .entries
            .contains_key(&at)
        {
            return;
        }
        shard
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .entries
            .remove(&at);
    }

    /// The lookups that found their page and those that did not, since the cache was made.
    pub(crate) fn hits_and_misses(&self) -> (u64, u64) {
        self.shards.iter().fold((0, 0), |(hits, misses), shard| {
            let shard = shard.read().unwrap_or_else(PoisonError::into_inner);
            (
                hits + shard.hits.load(Ordering::Relaxed),
                misses + shard.misses.load(Ordering::Relaxed),
            )
        })
    }

    #[cfg(test)]
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
