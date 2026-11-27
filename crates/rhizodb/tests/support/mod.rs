#![allow(
    dead_code,
    reason = "each test binary uses a different subset of the helpers"
)]

use std::collections::BTreeMap;
use std::io::Write;

use rhizodb::Database;
use rhizodb::ErrorKind;
use rhizodb::OpenOptions;
use rhizodb::Parameters;
use rhizodb::Value;

/// The settings every test database opens with. With `RHIZODB_TEST_PARALLEL` set, a read query
/// splits any match of two or more start nodes across four threads.
pub fn options() -> OpenOptions {
    match std::env::var_os("RHIZODB_TEST_PARALLEL") {
        Some(_) => parallel_options(),
        None => OpenOptions::new(),
    }
}

/// Settings that split every read query that can be split.
pub fn parallel_options() -> OpenOptions {
    OpenOptions::new().threads(4).parallel_threshold(2)
}

pub fn db() -> Database {
    options().in_memory().unwrap()
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
/// and otherwise one drawn from the clock. It is written to stderr before the test uses it, past
/// the test harness's capture, so a test that hangs or aborts still names its seed.
pub fn seed() -> u64 {
    let seed = std::env::var("RHIZODB_TEST_SEED")
        .ok()
        .and_then(|seed| seed.parse().ok())
        .unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos() as u64)
        });
    let thread = std::thread::current();
    let _ = writeln!(
        std::io::stderr(),
        "{} runs with RHIZODB_TEST_SEED={seed}",
        thread.name().unwrap_or("a test")
    );
    seed
}

/// The seeds listed one per line in `text`, skipping blank lines.
pub fn parse_seeds(text: &str) -> Vec<u64> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| {
            line.parse()
                .unwrap_or_else(|_| panic!("`{line}` is not a seed"))
        })
        .collect()
}

/// The seeds checked in under `tests/seed-regressions/<test>.txt`, each one a failure that
/// `RHIZODB_TEST_SEED` once replayed, so every run replays it before its own cases.
pub fn regression_seeds(test: &str) -> Vec<u64> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/seed-regressions")
        .join(format!("{test}.txt"));
    match std::fs::read_to_string(&path) {
        Ok(text) => parse_seeds(&text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => panic!("{}: {error}", path.display()),
    }
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

const ISOLATED_ENV: &str = "RHIZODB_ISOLATED_TEST";

/// Runs the test named `test` again in a child process, so a stack overflow, an abort, or a hang
/// in it fails this test instead of ending or stalling the whole run. Returns true in the child,
/// where the test then does its work, and false in the parent once the child passed.
pub fn isolated(test: &str, limit: std::time::Duration) -> bool {
    if std::env::var(ISOLATED_ENV).is_ok_and(|name| name == test) {
        return true;
    }
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            test,
            "--exact",
            "--include-ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(ISOLATED_ENV, test)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = std::io::Read::read_to_string(&mut stderr, &mut text);
        text
    });
    let started = std::time::Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if started.elapsed() > limit {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("{test} did not finish within {limit:?}");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    let stderr = reader.join().unwrap();
    let tail: String = stderr
        .lines()
        .rev()
        .take(15)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n");
    assert!(status.success(), "{test} ended with {status}:\n{tail}");
    false
}

/// Runs `work` on a thread with 1 MiB of stack, the most a query may use, which leaves a host
/// half of the 2 MiB stack Rust gives a spawned thread by default.
pub fn on_small_stack<T: Send>(work: impl FnOnce() -> T + Send) -> T {
    on_stack(1 << 20, work)
}

/// Runs `work` on a thread with `bytes` of stack.
pub fn on_stack<T: Send>(bytes: usize, work: impl FnOnce() -> T + Send) -> T {
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .stack_size(bytes)
            .spawn_scoped(scope, work)
            .unwrap()
            .join()
            .unwrap()
    })
}
