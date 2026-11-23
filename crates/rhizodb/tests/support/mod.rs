#![allow(
    dead_code,
    reason = "each test binary uses a different subset of the helpers"
)]

use std::collections::BTreeMap;

use rhizodb::Database;
use rhizodb::ErrorKind;
use rhizodb::Parameters;
use rhizodb::Value;

pub fn db() -> Database {
    Database::in_memory().unwrap()
}

pub fn run(db: &Database, query: &str) -> Vec<Vec<Value>> {
    match db.execute(query, &Parameters::new()) {
        Ok(result) => result.rows().to_vec(),
        Err(error) => panic!("{query}: {error} ({:?})", error.kind()),
    }
}

pub fn columns(db: &Database, query: &str) -> Vec<String> {
    db.execute(query, &Parameters::new())
        .unwrap()
        .columns()
        .to_vec()
}

pub fn single(db: &Database, query: &str) -> Value {
    let rows = run(db, query);
    assert_eq!(rows.len(), 1, "{query} returned {rows:?}");
    assert_eq!(rows[0].len(), 1, "{query} returned {rows:?}");
    rows[0][0].clone()
}

pub fn error_kind(db: &Database, query: &str) -> ErrorKind {
    match db.execute(query, &Parameters::new()) {
        Ok(result) => panic!("{query} succeeded with {:?}", result.rows()),
        Err(error) => error.kind(),
    }
}

pub fn int(value: i64) -> Value {
    Value::Integer(value)
}

pub fn float(value: f64) -> Value {
    Value::Float(value)
}

pub fn string(value: &str) -> Value {
    Value::String(value.to_owned())
}

pub fn boolean(value: bool) -> Value {
    Value::Boolean(value)
}

pub fn list(items: impl IntoIterator<Item = Value>) -> Value {
    Value::List(items.into_iter().collect())
}

pub fn map(entries: &[(&str, Value)]) -> Value {
    Value::Map(
        entries
            .iter()
            .map(|(key, value)| ((*key).to_owned(), value.clone()))
            .collect::<BTreeMap<_, _>>(),
    )
}

pub fn ints(rows: &[Vec<Value>]) -> Vec<i64> {
    rows.iter()
        .map(|row| match row.as_slice() {
            [Value::Integer(value)] => *value,
            other => panic!("expected one integer, got {other:?}"),
        })
        .collect()
}

/// `fast` cases, the count CI runs, times the scale that `RHIZODB_TEST_SCALE` sets for nightly
/// runs.
pub fn cases(fast: u64) -> u64 {
    let scale = std::env::var("RHIZODB_TEST_SCALE")
        .ok()
        .and_then(|scale| scale.parse::<u64>().ok())
        .unwrap_or(1);
    fast.saturating_mul(scale.max(1))
}

/// The seed a randomized test runs with: `RHIZODB_TEST_SEED` when set, so a failure replays,
/// and otherwise one drawn from the clock.
pub fn seed() -> u64 {
    std::env::var("RHIZODB_TEST_SEED")
        .ok()
        .and_then(|seed| seed.parse().ok())
        .unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos() as u64)
        })
}

/// SplitMix64, a small seeded generator, so randomized tests need no extra crate.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// A number in `0..bound`.
    pub fn below(&mut self, bound: u64) -> u64 {
        self.next_u64() % bound
    }

    /// True with probability `numerator / denominator`.
    pub fn chance(&mut self, numerator: u64, denominator: u64) -> bool {
        self.below(denominator) < numerator
    }

    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len() as u64) as usize]
    }
}
