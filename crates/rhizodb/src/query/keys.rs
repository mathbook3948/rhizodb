use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::hash::BuildHasherDefault;
use std::hash::Hash;
use std::hash::Hasher;

use crate::error::Error;
use crate::graph::Text;

use super::compare::Key;
use super::compare::order;
use super::datum::Datum;

const TWO_TO_63: f64 = 9_223_372_036_854_775_808.0;

/// Rows of values indexed by equivalence, which [`order`] decides: rows of scalars by a hash
/// that equivalent values share, every other row in order.
#[derive(Debug, Default)]
pub(crate) struct KeyIndex {
    state: RandomState,
    hashed: HashMap<u64, Bucket, BuildHasherDefault<Hashed>>,
    ordered: BTreeMap<Key, usize>,
}

type Bucket = Vec<(Vec<Datum>, usize)>;

#[derive(Debug, Default)]
struct Hashed(u64);

impl Hasher for Hashed {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 = self.0.rotate_left(8) ^ u64::from(*byte);
        }
    }

    fn write_u64(&mut self, value: u64) {
        self.0 = value;
    }
}

/// One value of a row looked up in a [`KeyIndex`], borrowed from where it is stored. Stored
/// text is checked as UTF-8 only when a row is kept.
#[derive(Clone, Debug)]
pub(crate) enum Probe<'a> {
    Text(Text<'a>),
    Value(Cow<'a, Datum>),
}

impl Probe<'_> {
    fn matches(&self, held: &Datum) -> bool {
        match self {
            Probe::Text(text) => {
                matches!(held, Datum::String(held) if held.as_bytes() == text.bytes())
            }
            Probe::Value(value) => order(held, value) == Ordering::Equal,
        }
    }

    pub(crate) fn owned(&self) -> Result<Datum, Error> {
        Ok(match self {
            Probe::Text(text) => Datum::String(text.to_str()?.to_owned()),
            Probe::Value(value) => value.as_ref().clone(),
        })
    }
}

impl KeyIndex {
    /// The value held for the row equivalent to `key`.
    pub(crate) fn get(&self, key: &[Datum]) -> Option<usize> {
        let mut hasher = self.state.build_hasher();
        if !key.iter().all(|value| hash_value(value, &mut hasher)) {
            return self.ordered.get(&Key(key.to_vec())).copied();
        }
        self.hashed.get(&hasher.finish()).and_then(|bucket| {
            bucket
                .iter()
                .find(|(held, _)| equivalent(held, key))
                .map(|(_, value)| *value)
        })
    }

    /// The value held for the row equivalent to `probe`.
    pub(crate) fn probe(&self, probe: &[Probe<'_>]) -> Result<Option<usize>, Error> {
        let mut hasher = self.state.build_hasher();
        let hashed = probe.iter().all(|value| match value {
            Probe::Text(text) => {
                hash_text(text.bytes(), &mut hasher);
                true
            }
            Probe::Value(value) => hash_value(value, &mut hasher),
        });
        if !hashed {
            let key = probe.iter().map(Probe::owned).collect::<Result<_, _>>()?;
            return Ok(self.ordered.get(&Key(key)).copied());
        }
        Ok(self.hashed.get(&hasher.finish()).and_then(|bucket| {
            bucket
                .iter()
                .find(|(held, _)| {
                    held.len() == probe.len()
                        && held
                            .iter()
                            .zip(probe)
                            .all(|(held, value)| value.matches(held))
                })
                .map(|(_, value)| *value)
        }))
    }

    /// Holds `value` for `key`, which no held row is equivalent to.
    pub(crate) fn insert(&mut self, key: Vec<Datum>, value: usize) {
        let mut hasher = self.state.build_hasher();
        if key.iter().all(|value| hash_value(value, &mut hasher)) {
            self.hashed
                .entry(hasher.finish())
                .or_default()
                .push((key, value));
        } else {
            self.ordered.insert(Key(key), value);
        }
    }
}

/// Rows of values, each kept once up to equivalence.
#[derive(Debug, Default)]
pub(crate) struct KeySet {
    index: KeyIndex,
}

impl KeySet {
    /// Keeps `key` and says whether no equivalent row was kept before.
    pub(crate) fn insert(&mut self, key: &[Datum]) -> bool {
        if self.index.get(key).is_some() {
            return false;
        }
        self.index.insert(key.to_vec(), 0);
        true
    }

    /// [`insert`](Self::insert) for a row of borrowed values, which are copied only when kept.
    pub(crate) fn insert_probe(&mut self, probe: &[Probe<'_>]) -> Result<bool, Error> {
        if self.index.probe(probe)?.is_some() {
            return Ok(false);
        }
        let key = probe.iter().map(Probe::owned).collect::<Result<_, _>>()?;
        self.index.insert(key, 0);
        Ok(true)
    }
}

fn equivalent(a: &[Datum], b: &[Datum]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| order(x, y) == Ordering::Equal)
}

fn hash_text(bytes: &[u8], hasher: &mut impl Hasher) {
    3u8.hash(hasher);
    hasher.write(bytes);
    hasher.write_u8(0xff);
}

fn hash_value(value: &Datum, hasher: &mut impl Hasher) -> bool {
    match value {
        Datum::Null => 0u8.hash(hasher),
        Datum::Boolean(value) => (1u8, value).hash(hasher),
        Datum::Integer(value) => (2u8, value).hash(hasher),
        Datum::Float(value) => float(*value, hasher),
        Datum::String(value) => hash_text(value.as_bytes(), hasher),
        Datum::Bytes(value) => (4u8, value).hash(hasher),
        Datum::Node(id) => (5u8, id.0).hash(hasher),
        Datum::Relationship(id) => (6u8, id.0).hash(hasher),
        Datum::List(_) | Datum::Map(_) | Datum::Path(_) | Datum::Temporal(_) => return false,
    }
    true
}

#[cfg(test)]
fn hash(state: &RandomState, key: &[Datum]) -> Option<u64> {
    let mut hasher = state.build_hasher();
    key.iter()
        .all(|value| hash_value(value, &mut hasher))
        .then(|| hasher.finish())
}

fn float(value: f64, hasher: &mut impl Hasher) {
    if value.is_nan() {
        7u8.hash(hasher);
    } else if value.trunc() == value && (-TWO_TO_63..TWO_TO_63).contains(&value) {
        (2u8, value as i64).hash(hasher);
    } else {
        (8u8, value.to_bits()).hash(hasher);
    }
}

#[cfg(test)]
#[path = "keys.test.rs"]
mod tests;
