use std::collections::BTreeMap;
use std::sync::Arc;

use crate::engine::block_file::FileSystem;
use crate::engine::sim_fs::Crash;
use crate::engine::sim_fs::Failure;
use crate::engine::sim_fs::SimFs;
use crate::engine::store::Engine;
use crate::engine::store::EngineOptions;
use crate::storage::ReadTransaction;
use crate::storage::Storage;
use crate::storage::Table;
use crate::storage::WriteTransaction;
use crate::test_scale;

const NAME: &str = "graph.rhizo";
const TABLES: [Table; 3] = [Table::Metadata, Table::NodeColumns, Table::PropertyEntries];

type State = BTreeMap<(Table, Vec<u8>), Vec<u8>>;

#[derive(Clone, Debug)]
enum Write {
    Put(Table, Vec<u8>, Vec<u8>),
    Delete(Table, Vec<u8>),
    DeleteRange(Table, Vec<u8>, Vec<u8>),
}

struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }
}

fn transactions(seed: u64, count: usize) -> Vec<Vec<Write>> {
    let mut random = Random(seed);
    (0..count)
        .map(|index| {
            let writes = 1 + random.below(6);
            (0..writes)
                .map(|_| {
                    let table = TABLES[random.below(3) as usize];
                    let key = vec![random.below(4) as u8, random.below(8) as u8];
                    match random.below(10) {
                        0 => Write::Delete(table, key),
                        1 => Write::DeleteRange(table, vec![key[0]], vec![key[0], 5]),
                        _ => {
                            let length = match random.below(12) {
                                0 => 3000,
                                1 => 70_000,
                                _ => random.below(40) as usize,
                            };
                            let value = (0..length).map(|at| (at + index) as u8).collect();
                            Write::Put(table, key, value)
                        }
                    }
                })
                .collect()
        })
        .collect()
}

fn applied(state: &State, writes: &[Write]) -> State {
    let mut state = state.clone();
    for write in writes {
        match write {
            Write::Put(table, key, value) => {
                state.insert((*table, key.clone()), value.clone());
            }
            Write::Delete(table, key) => {
                state.remove(&(*table, key.clone()));
            }
            Write::DeleteRange(table, start, end) => {
                state.retain(|(held, key), _| held != table || key < start || key >= end);
            }
        }
    }
    state
}

fn states(transactions: &[Vec<Write>]) -> Vec<State> {
    let mut states = vec![State::new()];
    for writes in transactions {
        let next = applied(states.last().unwrap(), writes);
        states.push(next);
    }
    states
}

fn dump(engine: &impl Storage) -> State {
    let read = engine.begin_read().unwrap();
    let mut state = State::new();
    for table in Table::ALL {
        for (key, value) in read.scan_prefix(table, &[]).unwrap() {
            state.insert((table, key), value);
        }
    }
    state
}

fn commit(engine: &impl Storage, writes: &[Write]) -> Result<(), crate::storage::StorageError> {
    let mut write = engine.begin_write()?;
    for op in writes {
        match op {
            Write::Put(table, key, value) => write.put(*table, key, value)?,
            Write::Delete(table, key) => write.delete(*table, key)?,
            Write::DeleteRange(table, start, end) => write.delete_range(*table, start, end)?,
        }
    }
    write.commit()
}

fn open(
    fs: &SimFs,
    options: EngineOptions,
) -> Result<Engine<SimFs>, crate::engine::error::EngineError> {
    Engine::open(Arc::new(fs.clone()), NAME, options)
}

fn options() -> EngineOptions {
    EngineOptions {
        segment_target: 8192,
    }
}

fn crashes(seeds: u64) -> Vec<Crash> {
    let mut crashes = vec![Crash::DropUnsynced, Crash::KeepUnsynced];
    for seed in 0..seeds {
        crashes.push(Crash::Scramble { seed });
        crashes.push(Crash::Reorder { seed });
    }
    crashes
}

struct Run {
    acknowledged: usize,
    in_flight: bool,
}

fn run_until_failure(engine: &Engine<SimFs>, transactions: &[Vec<Write>]) -> Run {
    for (index, writes) in transactions.iter().enumerate() {
        if commit(engine, writes).is_err() {
            assert!(
                commit(engine, &transactions[0]).is_err(),
                "a failed commit left the engine accepting writes"
            );
            return Run {
                acknowledged: index,
                in_flight: true,
            };
        }
    }
    Run {
        acknowledged: transactions.len(),
        in_flight: false,
    }
}

#[test]
fn every_failure_then_power_loss_keeps_each_acknowledged_commit() {
    let transactions = transactions(7, 24);
    let states = states(&transactions);
    let total = {
        let fs = SimFs::new();
        let engine = open(&fs, options()).unwrap();
        let before = fs.operations();
        run_until_failure(&engine, &transactions);
        fs.operations() - before
    };
    let seeds = u64::from(test_scale::cases(3));
    let mut outcomes = [0usize; 2];
    for fail_at in 0..=total {
        let fs = SimFs::new();
        let engine = open(&fs, options()).unwrap();
        fs.fail_after(fail_at, Failure::FromThenOn);
        let run = run_until_failure(&engine, &transactions);
        drop(engine);
        for crash in crashes(seeds) {
            let crashed = fs.crash(crash);
            let engine = open(&crashed, options())
                .unwrap_or_else(|error| panic!("fail at {fail_at}, {crash:?}: {error}"));
            let found = dump(&engine);
            let kept_in_flight = run.in_flight && found == states[run.acknowledged + 1];
            assert!(
                found == states[run.acknowledged] || kept_in_flight,
                "fail at {fail_at}, {crash:?}: {} commits acknowledged, state differs",
                run.acknowledged
            );
            outcomes[usize::from(kept_in_flight)] += 1;
            let recovered = if kept_in_flight {
                run.acknowledged + 1
            } else {
                run.acknowledged
            };
            let extra = &transactions[recovered % transactions.len()];
            commit(&engine, extra).unwrap();
            let expected = applied(&states[recovered], extra);
            drop(engine);
            let reopened = open(&crashed.crash(Crash::DropUnsynced), options()).unwrap();
            assert_eq!(dump(&reopened), expected, "fail at {fail_at}, {crash:?}");
        }
    }
    assert!(total > 50, "the workload ran only {total} operations");
    assert!(outcomes[0] > 0 && outcomes[1] > 0, "{outcomes:?}");
}

#[test]
fn every_failure_while_opening_leaves_the_database_whole() {
    let transactions = transactions(11, 12);
    let states = states(&transactions);
    let fs = SimFs::new();
    let engine = open(&fs, options()).unwrap();
    for writes in &transactions {
        commit(&engine, writes).unwrap();
    }
    drop(engine);
    let base = fs.crash(Crash::DropUnsynced);
    let total = {
        let copy = base.crash(Crash::DropUnsynced);
        let before = copy.operations();
        drop(open(&copy, options()).unwrap());
        copy.operations() - before
    };
    for fail_at in 0..=total {
        let copy = base.crash(Crash::DropUnsynced);
        copy.fail_after(fail_at, Failure::FromThenOn);
        let opened = open(&copy, options());
        drop(opened);
        for crash in crashes(2) {
            let engine = open(&copy.crash(crash), options())
                .unwrap_or_else(|error| panic!("fail at {fail_at}, {crash:?}: {error}"));
            assert_eq!(
                dump(&engine),
                states[transactions.len()],
                "fail at {fail_at}, {crash:?}"
            );
        }
    }
}

#[test]
fn a_new_database_appears_whole_or_not_at_all() {
    let total = {
        let fs = SimFs::new();
        drop(open(&fs, options()).unwrap());
        fs.operations()
    };
    for fail_at in 0..=total {
        let fs = SimFs::new();
        fs.fail_after(fail_at, Failure::FromThenOn);
        drop(open(&fs, options()));
        for crash in crashes(2) {
            let crashed = fs.crash(crash);
            let engine = open(&crashed, options())
                .unwrap_or_else(|error| panic!("fail at {fail_at}, {crash:?}: {error}"));
            assert!(dump(&engine).is_empty());
            commit(
                &engine,
                &[Write::Put(Table::Metadata, b"k".to_vec(), b"v".to_vec())],
            )
            .unwrap();
            drop(engine);
            let engine = open(&crashed.crash(Crash::DropUnsynced), options()).unwrap();
            assert_eq!(dump(&engine).len(), 1);
            assert!(
                !crashed
                    .names()
                    .unwrap()
                    .iter()
                    .any(|name| name.ends_with("-init"))
            );
        }
    }
}
