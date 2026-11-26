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

#[test]
fn a_file_cut_short_of_its_recorded_length_fails_to_open_as_corrupt_data() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("cut.rhizodb");
    {
        let db = Database::open(&path).unwrap();
        run(
            &db,
            "UNWIND range(1, 3000) AS i INSERT (:Item {i: i, text: 'word ' + toString(i)})",
        );
    }
    let whole = std::fs::read(&path).unwrap();
    let mut cuts: Vec<usize> = [8192, 12288, 98_304, whole.len() / 2, whole.len() - 1]
        .into_iter()
        .filter(|&cut| cut > 4096 && cut < whole.len())
        .collect();
    cuts.dedup();
    assert!(cuts.len() >= 3, "the file is {} bytes", whole.len());
    for cut in cuts {
        std::fs::write(&path, &whole[..cut]).unwrap();
        match Database::open(&path) {
            Ok(db) => {
                let failure = db
                    .execute("MATCH (n:Item) RETURN n.text", &rhizodb::Parameters::new())
                    .err()
                    .map(|error| error.kind());
                assert!(
                    failure.is_none_or(|kind| kind == ErrorKind::CorruptData),
                    "cut at {cut}: {failure:?}"
                );
            }
            Err(error) => assert_eq!(
                error.kind(),
                ErrorKind::CorruptData,
                "cut at {cut}: {error}"
            ),
        }
    }
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

/// Flips one bit of every copy of `needle` in the file, since the storage engine may keep the
/// bytes of a page it copied, and returns how many there were.
fn flip_every(file: &mut [u8], needle: &[u8]) -> usize {
    let mut found = 0;
    let mut at = 0;
    while let Some(offset) = file[at..]
        .windows(needle.len())
        .position(|window| window == needle)
    {
        let start = at + offset;
        file[start + needle.len() / 2] ^= 0x01;
        found += 1;
        at = start + needle.len();
    }
    found
}

/// Holds the adjacency of 4000 nodes, so the adjacency of node 0 and of node 3000 lie on pages
/// apart, and the storage engine, which checksums each page, fails only the reads of the page a
/// damaged byte is on.
fn adjacency_file(path: &Path) {
    let db = Database::open(path).unwrap();
    run(&db, "UNWIND range(0, 199) AS i INSERT (:Item {i: i})");
    run(
        &db,
        "MATCH (a:Item {i: 0}), (b:Item) WHERE b.i >= 100 AND b.i < 120 INSERT (a)-[:T]->(b)",
    );
    run(
        &db,
        "MATCH (a:Item), (b:Item) WHERE a.i >= 1 AND a.i < 99 AND b.i = a.i + 1 INSERT (a)-[:T]->(b)",
    );
    run(
        &db,
        "UNWIND range(100, 1999) AS i INSERT (:Item {i: 2 * i})-[:T]->(:Item {i: 2 * i + 1})",
    );
}

#[test]
fn a_damaged_adjacency_part_fails_as_corrupt_data_where_it_is_read() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("adjacency.rhizodb");
    adjacency_file(&path);
    let mut file = std::fs::read(&path).unwrap();
    let needle: Vec<u8> = (101..107).flat_map(|other| [1, other]).collect();
    assert!(
        flip_every(&mut file, &needle) > 0,
        "the part keeps the relationships of one node side by side"
    );
    std::fs::write(&path, &file).unwrap();
    let db = Database::open(&path).unwrap();
    for query in [
        "MATCH (a:Item {i: 0})-[]->(b) RETURN b.i",
        "MATCH (a:Item {i: 0})-[:T]->(b) RETURN count(b)",
        "MATCH (a:Item {i: 0})-[]-(b) RETURN count(b)",
        "MATCH (a:Item)-[:T]->(b) RETURN count(*)",
        "MATCH (a:Item {i: 0}) DETACH DELETE a",
    ] {
        assert_eq!(error_kind(&db, query), ErrorKind::CorruptData, "{query}");
    }
    assert_eq!(
        run(
            &db,
            "MATCH (b:Item {i: 3001}) MATCH (b)<-[:T]-(a) RETURN a.i"
        ),
        [vec![int(3000)]],
        "the adjacency of a node on another page still reads"
    );
}

#[test]
fn a_damaged_adjacency_delta_fails_as_corrupt_data_where_it_is_read() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("adjacency.rhizodb");
    adjacency_file(&path);
    {
        let db = Database::open(&path).unwrap();
        run(
            &db,
            "MATCH (a:Item {i: 0}), (b:Item) WHERE b.i >= 60 AND b.i < 63 INSERT (a)-[:T]->(b)",
        );
        assert_eq!(
            run(&db, "MATCH (a:Item {i: 0})-[:T]->(b) RETURN count(b)"),
            [vec![int(23)]]
        );
    }
    let mut file = std::fs::read(&path).unwrap();
    assert!(
        flip_every(&mut file, &[60, 1, 61, 1, 62]) > 0,
        "the delta lists the relationships it adds side by side"
    );
    std::fs::write(&path, &file).unwrap();
    let db = Database::open(&path).unwrap();
    for query in [
        "MATCH (a:Item {i: 0})-[]->(b) RETURN b.i",
        "MATCH (a:Item {i: 0})-[:T]->(b) RETURN count(b)",
        "MATCH (a:Item {i: 0})-[r]->(b) DELETE r",
    ] {
        assert_eq!(error_kind(&db, query), ErrorKind::CorruptData, "{query}");
    }
    assert_eq!(
        run(&db, "MATCH (a:Item {i: 3000})-[:T]->(b) RETURN b.i"),
        [vec![int(3001)]],
        "the adjacency of a node on another page still reads"
    );
}
