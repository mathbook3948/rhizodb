//! Random write transactions through GQL, compared after every transaction with a plain model
//! of the graph, together with read queries the model answers on its own: filtered counts with
//! and without a property index, ternary partitions of a predicate, pattern counts, and ordered
//! projections.

#[path = "support/graph_ops.rs"]
mod graph_ops;
mod support;

use graph_ops::ByteChoices;
use graph_ops::Choices;
use graph_ops::Run;
use rhizodb::Database;
use support::*;

const FAST_RUNS: u64 = 16;
const TRANSACTIONS_PER_RUN: u64 = 40;

impl Choices for Rng {
    fn below(&mut self, bound: u64) -> u64 {
        Rng::below(self, bound)
    }
}

fn fail(seed: u64, run: &Run, reason: &str) -> ! {
    panic!(
        "seed {seed} (replay with RHIZODB_TEST_SEED={seed}): {reason}\nstatements:\n{}",
        run.log()
    )
}

#[test]
fn random_transactions_in_memory_match_the_model() {
    let base = seed();
    for offset in 0..cases(FAST_RUNS) {
        let seed = base.wrapping_add(offset);
        let mut rng = Rng::new(seed);
        let mut run = Run::new(Database::in_memory().unwrap());
        for _ in 0..TRANSACTIONS_PER_RUN {
            if let Err(reason) = run.transaction(&mut rng) {
                fail(seed, &run, &reason);
            }
        }
    }
}

#[test]
fn random_transactions_on_a_file_match_the_model_across_reopens() {
    let base = seed();
    let directory = tempfile::tempdir().unwrap();
    for offset in 0..cases(FAST_RUNS / 4) {
        let seed = base.wrapping_add(offset);
        let path = directory.path().join(format!("run-{offset}.rhizodb"));
        let mut rng = Rng::new(seed);
        let mut run = Run::new(Database::open(&path).unwrap());
        for _ in 0..TRANSACTIONS_PER_RUN {
            let step = if rng.chance(1, 8) {
                run.reopen(|| Database::open(&path).unwrap())
            } else {
                run.transaction(&mut rng)
            };
            if let Err(reason) = step {
                fail(seed, &run, &reason);
            }
        }
    }
}

#[test]
fn checked_in_fuzzer_inputs_match_the_model() {
    let directory =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fuzz-regressions/graph_ops");
    for entry in std::fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        let bytes = std::fs::read(&path).unwrap();
        let mut choices = ByteChoices::new(&bytes);
        let mut run = Run::new(Database::in_memory().unwrap());
        while !choices.exhausted() {
            if let Err(reason) = run.transaction(&mut choices) {
                panic!("{}: {reason}\nstatements:\n{}", path.display(), run.log());
            }
        }
    }
}
