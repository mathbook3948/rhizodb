use std::collections::HashMap;
use std::hash::BuildHasherDefault;
use std::hash::Hash;
use std::hash::Hasher;

/// How many bytes each cache of a query's readers holds before it drops what it used longest
/// ago. The budget is per map, so a column reader and a label reader together hold about six
/// times it. A group's column of 200-byte strings takes about 800 KiB, and its node meta chunk
/// at most about 9 KiB.
pub(crate) const BUDGET: usize = 8 << 20;

/// What a cached value costs to hold, in bytes, besides its entry.
pub(crate) trait Weigh {
    fn weight(&self) -> usize;
}

/// A map that holds about `budget` bytes. When an insert would pass the budget, it drops the
/// entries used longest ago until half the budget is left, so each insert costs amortized
/// logarithmic time. The entry just inserted always stays, so the map passes its budget by at
/// most that one entry.
#[derive(Debug)]
pub(crate) struct Budgeted<K, V> {
    entries: HashMap<K, Entry<V>, BuildHasherDefault<KeyHasher>>,
    clock: u64,
    bytes: usize,
    budget: usize,
}

#[derive(Debug)]
struct Entry<V> {
    value: V,
    used: u64,
    weight: usize,
}

impl<K, V> Default for Budgeted<K, V> {
    fn default() -> Self {
        Self::new(BUDGET)
    }
}

impl<K, V> Budgeted<K, V> {
    pub(crate) fn new(budget: usize) -> Self {
        Self {
            entries: HashMap::default(),
            clock: 0,
            bytes: 0,
            budget,
        }
    }

    /// The bytes the map holds, counting each entry.
    #[cfg(test)]
    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }

    #[cfg(test)]
    pub(crate) fn values(&self) -> impl Iterator<Item = &V> {
        self.entries.values().map(|entry| &entry.value)
    }
}

impl<K: Copy + Eq + Hash, V: Weigh> Budgeted<K, V> {
    pub(crate) fn get(&mut self, key: &K) -> Option<&mut V> {
        self.clock += 1;
        let entry = self.entries.get_mut(key)?;
        entry.used = self.clock;
        Some(&mut entry.value)
    }

    /// Holds `value` for `key` in place of what it held before.
    pub(crate) fn insert(&mut self, key: K, value: V) -> &mut V {
        if let Some(old) = self.entries.remove(&key) {
            self.bytes -= old.weight;
        }
        let weight = size_of::<(K, Entry<V>)>() + value.weight();
        if self.bytes + weight > self.budget {
            self.shrink((self.budget / 2).saturating_sub(weight));
        }
        self.clock += 1;
        self.bytes += weight;
        &mut self
            .entries
            .entry(key)
            .insert_entry(Entry {
                value,
                used: self.clock,
                weight,
            })
            .into_mut()
            .value
    }

    fn shrink(&mut self, to: usize) {
        let mut by_use: Vec<(u64, K, usize)> = self
            .entries
            .iter()
            .map(|(key, entry)| (entry.used, *key, entry.weight))
            .collect();
        by_use.sort_unstable_by_key(|(used, _, _)| *used);
        for (_, key, weight) in by_use {
            if self.bytes <= to {
                break;
            }
            self.entries.remove(&key);
            self.bytes -= weight;
        }
    }
}

/// Mixes the integer fields of a key, which are group numbers and tokens.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct KeyHasher(u64);

impl Hasher for KeyHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.write_u64(u64::from(*byte));
        }
    }

    fn write_u32(&mut self, value: u32) {
        self.write_u64(u64::from(value));
    }

    fn write_u64(&mut self, value: u64) {
        self.0 = (self.0 ^ value)
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .rotate_left(29);
    }
}

#[cfg(test)]
#[path = "budget.test.rs"]
mod tests;
