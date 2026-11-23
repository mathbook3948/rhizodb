use std::collections::HashMap;
use std::hash::BuildHasherDefault;
use std::hash::Hasher;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;

use super::layout::BlockRef;
use super::page::PageHeader;

/// A checked page and its header, which never changes once written.
pub(crate) type CachedPage = (PageHeader, Arc<[u8]>);

const SHARDS: usize = 16;

#[derive(Debug)]
struct Entry {
    at: BlockRef,
    page: CachedPage,
    used: bool,
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
    shards: Vec<Mutex<Shard>>,
    per_shard: usize,
}

impl Cache {
    /// A cache holding about `pages` pages.
    pub(crate) fn new(pages: usize) -> Self {
        Self {
            shards: (0..SHARDS).map(|_| Mutex::default()).collect(),
            per_shard: pages.div_ceil(SHARDS).max(1),
        }
    }

    fn shard(&self, at: BlockRef) -> std::sync::MutexGuard<'_, Shard> {
        let mixed = at.raw().wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 58;
        self.shards[mixed as usize % SHARDS]
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn get(&self, at: BlockRef) -> Option<CachedPage> {
        let mut shard = self.shard(at);
        let index = *shard.index.get(&at)?;
        let entry = &mut shard.entries[index];
        entry.used = true;
        Some(entry.page.clone())
    }

    pub(crate) fn insert(&self, at: BlockRef, page: CachedPage) {
        let per_shard = self.per_shard;
        let mut shard = self.shard(at);
        if shard.index.contains_key(&at) {
            return;
        }
        if shard.entries.len() < per_shard {
            let index = shard.entries.len();
            shard.entries.push(Entry {
                at,
                page,
                used: false,
            });
            shard.index.insert(at, index);
            return;
        }
        loop {
            let hand = shard.hand;
            shard.hand = (hand + 1) % shard.entries.len();
            let entry = &mut shard.entries[hand];
            if entry.used {
                entry.used = false;
                continue;
            }
            let evicted = entry.at;
            *entry = Entry {
                at,
                page,
                used: false,
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
                    .lock()
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
