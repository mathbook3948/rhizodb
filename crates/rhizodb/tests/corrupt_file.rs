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

use rhizodb::Database;
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
    probe_on(&pristine, inputs)
}

/// The undamaged `db_file` target file of format 8, which the checked-in fuzzer inputs were
/// found on. Its damages hit the same bytes there, and the open upgrades what is left.
fn format_eight_pristine() -> Vec<u8> {
    std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/format-8-db-file.redb"),
    )
    .unwrap()
}

fn probe_on(pristine: &[u8], inputs: impl IntoIterator<Item = (String, Vec<u8>)>) -> Outcomes {
    let directory = tempfile::tempdir().unwrap();
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
        let file = db_file::damage(pristine.to_vec(), &bytes);
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

fn fuzzer_inputs() -> Vec<(String, Vec<u8>)> {
    regressions("db_file")
        .into_iter()
        .map(|path| (path.display().to_string(), std::fs::read(&path).unwrap()))
        .collect()
}

#[test]
fn every_fuzzer_input_fails_without_a_panic_on_the_current_file() {
    let outcomes = probe(fuzzer_inputs());
    assert!(outcomes.panics.is_empty(), "{}", outcomes.panics.join("\n"));
}

#[test]
fn the_undamaged_format_eight_file_upgrades_and_answers_every_probe() {
    let outcomes = probe_on(
        &format_eight_pristine(),
        [("undamaged".to_owned(), Vec::new())],
    );
    assert!(outcomes.panics.is_empty(), "{}", outcomes.panics.join("\n"));
    assert_eq!(
        outcomes.failures,
        vec![("undamaged".to_owned(), Vec::new())]
    );
}

#[test]
fn every_fuzzer_input_that_damaged_a_file_fails_as_corrupt_data_without_a_panic() {
    let outcomes = probe_on(&format_eight_pristine(), fuzzer_inputs());
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

/// Flips one bit of the first copy of `needle` in the file and returns whether there was one.
fn flip_inside(file: &mut [u8], needle: &[u8]) -> bool {
    let Some(at) = file
        .windows(needle.len())
        .position(|window| window == needle)
    else {
        return false;
    };
    file[at + needle.len() / 2] ^= 0x01;
    true
}

#[test]
fn a_damaged_property_column_fails_as_corrupt_data_where_it_is_read() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("columns.rhizodb");
    {
        let db = Database::open(&path).unwrap();
        run(
            &db,
            "UNWIND range(1, 500) AS i INSERT (:Item {i: i, text: 'word ' + toString(i)}), (:Other {i: i})",
        );
    }
    let mut file = std::fs::read(&path).unwrap();
    assert!(
        flip_inside(&mut file, b"word 300word 301word 302"),
        "the string column keeps its values side by side"
    );
    std::fs::write(&path, &file).unwrap();
    let db = Database::open(&path).unwrap();
    for query in [
        "MATCH (n:Item) RETURN n.text",
        "MATCH (n:Item) WHERE n.text = 'word 301' RETURN n.i",
        "MATCH (n:Item {i: 301}) RETURN n.text",
        "MATCH (n:Item) RETURN n ORDER BY n.i",
    ] {
        assert_eq!(error_kind(&db, query), ErrorKind::CorruptData, "{query}");
    }
    assert_eq!(
        run(&db, "MATCH (n:Item) RETURN count(n), sum(n.i)"),
        [vec![int(500), int(125_250)]],
        "a read of other columns is unaffected"
    );
}
