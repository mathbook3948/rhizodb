use std::collections::HashMap;
use std::collections::HashSet;
use std::hash::BuildHasherDefault;
use std::hash::Hasher;
use std::sync::Arc;
use std::sync::PoisonError;
use std::sync::RwLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use super::blob::BlobRef;
use super::layout::BLOCK;
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

const CLOCK_SLACK: usize = 64;

const ENTRY_BYTES: usize =
    size_of::<(BlockRef, Entry)>() + size_of::<BlockRef>() + 2 * size_of::<usize>();

/// Whether a read looked up one key or belongs to a long scan, which decides whether what it
/// reads may push other entries out of the cache.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Access {
    Point,
    Scan,
}

const SHORT_SCAN: u64 = 1 << 20;

impl Access {
    /// The access of a scan's next read of `bytes`, after adding them to the bytes `read` the
    /// scan has read.
    pub(crate) fn of_scan(read: &mut u64, bytes: usize) -> Self {
        *read += bytes as u64;
        if *read > SHORT_SCAN {
            Access::Scan
        } else {
            Access::Point
        }
    }
}

#[derive(Debug)]
enum Cached {
    Page(CachedPage),
    Value(BlobRef, Arc<[u8]>),
}

impl Cached {
    fn weight(&self) -> usize {
        ENTRY_BYTES
            + match self {
                Cached::Page(_) => size_of::<CheckedPage>() + BLOCK,
                Cached::Value(_, bytes) => bytes.len(),
            }
    }
}

#[derive(Debug)]
struct Entry {
    cached: Cached,
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
    bytes: usize,
    hits: AtomicU64,
    misses: AtomicU64,
}

/// A sharded CLOCK cache of checked pages and values keyed by block reference, which holds
/// about its budget in bytes. A block takes a new page only once no tree reaches its old one,
/// and writing it removes what the cache held for it.
#[derive(Debug)]
pub(crate) struct Cache {
    shards: Vec<RwLock<Shard>>,
    per_shard: usize,
}

impl Cache {
    /// A cache holding about `bytes` bytes of pages and values.
    pub(crate) fn new(bytes: usize) -> Self {
        Self {
            shards: (0..SHARDS).map(|_| RwLock::default()).collect(),
            per_shard: bytes.div_ceil(SHARDS).max(BLOCK),
        }
    }

    fn shard(&self, at: BlockRef) -> &RwLock<Shard> {
        let mixed = at.raw().wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 58;
        &self.shards[mixed as usize % SHARDS]
    }

    fn find<T>(&self, at: BlockRef, take: impl FnOnce(&Cached) -> Option<T>) -> Option<T> {
        let shard = self
            .shard(at)
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        let Some((entry, found)) = shard
            .entries
            .get(&at)
            .and_then(|entry| Some((entry, take(&entry.cached)?)))
        else {
            shard.misses.fetch_add(1, Ordering::Relaxed);
            return None;
        };
        shard.hits.fetch_add(1, Ordering::Relaxed);
        if !entry.used.load(Ordering::Relaxed) {
            entry.used.store(true, Ordering::Relaxed);
        }
        Some(found)
    }

    pub(crate) fn get(&self, at: BlockRef) -> Option<CachedPage> {
        self.find(at, |cached| match cached {
            Cached::Page(page) => Some(Arc::clone(page)),
            Cached::Value(..) => None,
        })
    }

    /// The value `blob` points to, when the cache holds it.
    pub(crate) fn value(&self, blob: &BlobRef) -> Option<Arc<[u8]>> {
        self.find(blob.first.at, |cached| match cached {
            Cached::Value(held, bytes) if held == blob => Some(Arc::clone(bytes)),
            _ => None,
        })
    }

    pub(crate) fn insert(&self, at: BlockRef, page: CachedPage, access: Access) {
        self.hold(at, Cached::Page(page), access);
    }

    /// Keeps the value `blob` points to, unless it would take more than a quarter of its
    /// shard, so one large value never empties a shard.
    pub(crate) fn insert_value(&self, blob: BlobRef, bytes: Arc<[u8]>, access: Access) {
        if bytes.len() <= self.per_shard / 4 {
            self.hold(blob.first.at, Cached::Value(blob, bytes), access);
        }
    }

    fn hold(&self, at: BlockRef, cached: Cached, access: Access) {
        let weight = cached.weight();
        let mut shard = self
            .shard(at)
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        let shard = &mut *shard;
        if shard.entries.contains_key(&at) {
            return;
        }
        if shard.clock.len() >= 2 * shard.entries.len() + CLOCK_SLACK {
            let entries = &shard.entries;
            let mut seen: HashSet<BlockRef, BuildHasherDefault<BlockHasher>> =
                HashSet::with_capacity_and_hasher(entries.len(), BuildHasherDefault::default());
            shard
                .clock
                .retain(|held| entries.contains_key(held) && seen.insert(*held));
            shard.hand = 0;
        }
        if access == Access::Scan && shard.bytes + weight > self.per_shard {
            return;
        }
        let mut freed = None;
        while shard.bytes > 0 && shard.bytes + weight > self.per_shard {
            let hand = shard.hand % shard.clock.len();
            shard.hand = hand + 1;
            let held = shard.clock[hand];
            match shard.entries.get_mut(&held) {
                Some(entry) if entry.used.load(Ordering::Relaxed) => {
                    *entry.used.get_mut() = false;
                    continue;
                }
                Some(_) => {
                    if let Some(entry) = shard.entries.remove(&held) {
                        shard.bytes -= entry.cached.weight();
                    }
                }
                None => {}
            }
            freed.get_or_insert(hand);
        }
        shard.bytes += weight;
        match freed {
            Some(place) => shard.clock[place] = at,
            None => shard.clock.push(at),
        }
        shard.entries.insert(
            at,
            Entry {
                cached,
                used: AtomicBool::new(false),
            },
        );
    }

    /// Forgets what the cache held for `at`, whose block is being written again. Its place on
    /// the clock stays until the hand reaches it or the clock is pruned, which happens once
    /// such places outnumber the entries.
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
        let mut shard = shard.write().unwrap_or_else(PoisonError::into_inner);
        if let Some(entry) = shard.entries.remove(&at) {
            shard.bytes -= entry.cached.weight();
        }
    }

    /// The lookups that found their page or value and those that did not, since the cache was
    /// made.
    pub(crate) fn hits_and_misses(&self) -> (u64, u64) {
        self.shards.iter().fold((0, 0), |(hits, misses), shard| {
            let shard = shard.read().unwrap_or_else(PoisonError::into_inner);
            (
                hits + shard.hits.load(Ordering::Relaxed),
                misses + shard.misses.load(Ordering::Relaxed),
            )
        })
    }

    /// The bytes of the pages and values the cache holds.
    #[cfg(test)]
    pub(crate) fn bytes(&self) -> usize {
        self.shards
            .iter()
            .map(|shard| shard.read().unwrap_or_else(PoisonError::into_inner).bytes)
            .sum()
    }

    /// The places on the shards' clocks, those of removed entries included.
    #[cfg(test)]
    pub(crate) fn places(&self) -> usize {
        self.shards
            .iter()
            .map(|shard| {
                shard
                    .read()
                    .unwrap_or_else(PoisonError::into_inner)
                    .clock
                    .len()
            })
            .sum()
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
