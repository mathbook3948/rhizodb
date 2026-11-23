//! Damaged database files must make `Database::open` or the query fail with an error, never
//! panic. The damage and the probes are shared with the `db_file` fuzz target, and every
//! input the fuzzer found a panic with is checked in under `fuzz-regressions/db_file`.

#[path = "support/db_file.rs"]
mod db_file;
mod support;

use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;

use support::*;

const FAST_DAMAGES: u64 = 24;

static LAST_PANIC: Mutex<String> = Mutex::new(String::new());

fn regressions(target: &str) -> Vec<PathBuf> {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fuzz-regressions")
        .join(target);
    let mut inputs: Vec<PathBuf> = std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    inputs.sort();
    inputs
}

/// Opens and probes every damaged file and returns the panics, each with the damage bytes that
/// caused it.
fn panics(inputs: impl IntoIterator<Item = (String, Vec<u8>)>) -> Vec<String> {
    let directory = tempfile::tempdir().unwrap();
    let pristine = db_file::pristine(&directory.path().join("pristine.rhizodb"));
    let damaged = directory.path().join("damaged.rhizodb");
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|info| {
        if let Some(location) = info.location() {
            *LAST_PANIC.lock().unwrap() = location.to_string();
        }
    }));
    let found = inputs
        .into_iter()
        .filter_map(|(name, bytes)| {
            let file = db_file::damage(pristine.clone(), &bytes);
            std::panic::catch_unwind(AssertUnwindSafe(|| {
                db_file::open_and_probe(&damaged, &file)
            }))
            .err()
            .map(|payload| {
                let message = payload
                    .downcast_ref::<&str>()
                    .map(|message| (*message).to_owned())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_default();
                let location = LAST_PANIC.lock().unwrap().clone();
                format!("{name} (damage {bytes:?}): {message} at {location}")
            })
        })
        .collect();
    std::panic::set_hook(hook);
    found
}

#[test]
#[ignore = "bug: redb panics with `entered unreachable code` in btree_cursor.rs while a query \
            reads a damaged b-tree page, so a corrupted file panics the host instead of \
            failing with ErrorKind::CorruptData"]
fn fuzzer_inputs_that_damaged_a_file_fail_without_a_panic() {
    let found = panics(
        regressions("db_file")
            .into_iter()
            .map(|path| (path.display().to_string(), std::fs::read(&path).unwrap())),
    );
    assert!(found.is_empty(), "{}", found.join("\n"));
}

#[test]
#[ignore = "bug: redb panics on damaged pages instead of returning an error, see \
            fuzzer_inputs_that_damaged_a_file_fail_without_a_panic"]
fn random_single_damages_fail_without_a_panic() {
    let seed = seed();
    let mut rng = Rng::new(seed);
    let inputs: Vec<(String, Vec<u8>)> = (0..cases(FAST_DAMAGES))
        .map(|index| {
            let bytes = rng.next_u64().to_le_bytes()[..db_file::DAMAGE_BYTES].to_vec();
            (format!("seed {seed} damage {index}"), bytes)
        })
        .collect();
    let found = panics(inputs);
    assert!(found.is_empty(), "{}", found.join("\n"));
}

#[test]
fn a_truncated_file_fails_to_open_or_reads_without_a_panic() {
    let inputs = (1..=16u32).map(|step| {
        let offset = step * 104_729;
        let mut bytes = vec![2, 0];
        bytes.extend_from_slice(&offset.to_le_bytes());
        (format!("truncated at {offset} modulo the length"), bytes)
    });
    let found = panics(inputs);
    assert!(found.is_empty(), "{}", found.join("\n"));
}
