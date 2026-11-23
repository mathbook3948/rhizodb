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

use rhizodb::ErrorKind;
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

struct Outcomes {
    panics: Vec<String>,
    failures: Vec<(String, Vec<ErrorKind>)>,
}

/// Opens and probes every damaged file, and returns the panics, each with the damage bytes that
/// caused it, and the kinds of the failures of each input that did not panic.
fn probe(inputs: impl IntoIterator<Item = (String, Vec<u8>)>) -> Outcomes {
    let directory = tempfile::tempdir().unwrap();
    let pristine = db_file::pristine(&directory.path().join("pristine.rhizodb"));
    let damaged = directory.path().join("damaged.rhizodb");
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|info| {
        if let Some(location) = info.location() {
            *LAST_PANIC.lock().unwrap() = location.to_string();
        }
    }));
    let mut outcomes = Outcomes {
        panics: Vec::new(),
        failures: Vec::new(),
    };
    for (name, bytes) in inputs {
        let file = db_file::damage(pristine.clone(), &bytes);
        match std::panic::catch_unwind(AssertUnwindSafe(|| {
            db_file::open_and_probe(&damaged, &file)
        })) {
            Ok(kinds) => outcomes.failures.push((name, kinds)),
            Err(payload) => {
                let message = payload
                    .downcast_ref::<&str>()
                    .map(|message| (*message).to_owned())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_default();
                let location = LAST_PANIC.lock().unwrap().clone();
                outcomes.panics.push(format!(
                    "{name} (damage {bytes:?}): {message} at {location}"
                ));
            }
        }
    }
    std::panic::set_hook(hook);
    outcomes
}

fn random_damages(count: usize) -> Vec<(String, Vec<u8>)> {
    let seed = seed();
    let mut rng = Rng::new(seed);
    (0..cases(FAST_DAMAGES))
        .map(|index| {
            let bytes = (0..count * db_file::DAMAGE_BYTES)
                .map(|_| rng.next_u64() as u8)
                .collect();
            (format!("seed {seed} damage {index}"), bytes)
        })
        .collect()
}

#[test]
fn every_fuzzer_input_that_damaged_a_file_fails_as_corrupt_data_without_a_panic() {
    let outcomes = probe(
        regressions("db_file")
            .into_iter()
            .map(|path| (path.display().to_string(), std::fs::read(&path).unwrap())),
    );
    assert!(outcomes.panics.is_empty(), "{}", outcomes.panics.join("\n"));
    let unreported: Vec<String> = outcomes
        .failures
        .iter()
        .filter(|(_, kinds)| !kinds.contains(&ErrorKind::CorruptData))
        .map(|(name, kinds)| format!("{name}: {kinds:?}"))
        .collect();
    assert!(unreported.is_empty(), "{}", unreported.join("\n"));
}

#[test]
fn random_single_damages_fail_without_a_panic() {
    let outcomes = probe(random_damages(1));
    assert!(outcomes.panics.is_empty(), "{}", outcomes.panics.join("\n"));
}

#[test]
fn random_multiple_damages_fail_without_a_panic() {
    let outcomes = probe(random_damages(db_file::MAX_DAMAGES));
    assert!(outcomes.panics.is_empty(), "{}", outcomes.panics.join("\n"));
}

#[test]
fn a_truncated_file_fails_to_open_or_reads_without_a_panic() {
    let inputs = (1..=16u32).map(|step| {
        let offset = step * 104_729;
        let mut bytes = vec![2, 0];
        bytes.extend_from_slice(&offset.to_le_bytes());
        (format!("truncated at {offset} modulo the length"), bytes)
    });
    let outcomes = probe(inputs);
    assert!(outcomes.panics.is_empty(), "{}", outcomes.panics.join("\n"));
}
