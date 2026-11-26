use std::collections::BTreeMap;
use std::sync::Arc;

use crate::engine::block_file::BlockFile;
use crate::engine::block_file::FileSystem;
use crate::engine::extent::Blocks;
use crate::engine::sim_fs::Crash;
use crate::engine::sim_fs::Failure;
use crate::engine::sim_fs::SimFs;
use crate::engine::store::CheckpointMode;
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
        memtable_limit: 6_000,
        wal_limit: 20_000,
        checkpoint: CheckpointMode::Inline,
        cache_pages: 64,
        reclaim_slack: 64 << 10,
        stage_floor: 8,
        ingest_limit: u64::MAX,
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
    fail_each_operation_then_lose_power(options(), 8);
}

#[test]
fn every_failure_then_power_loss_keeps_each_commit_checkpointed_at_once() {
    let every_commit = EngineOptions {
        memtable_limit: 0,
        ..options()
    };
    fail_each_operation_then_lose_power(every_commit, 20);
}

#[test]
fn every_failure_then_power_loss_keeps_each_commit_that_went_straight_into_the_file() {
    let ingesting = EngineOptions {
        ingest_limit: 2_500,
        ..options()
    };
    fail_each_operation_then_lose_power(ingesting, 8);
}

fn fail_each_operation_then_lose_power(options: EngineOptions, minimum_generation: u64) {
    let transactions = transactions(7, 24);
    let states = states(&transactions);
    let total = {
        let fs = SimFs::new();
        let engine = open(&fs, options).unwrap();
        let before = fs.operations();
        run_until_failure(&engine, &transactions);
        assert!(
            engine.header().generation >= minimum_generation,
            "the workload checkpointed too rarely"
        );
        drop(engine);
        fs.operations() - before
    };
    let seeds = u64::from(test_scale::cases(3));
    let mut outcomes = [0usize; 2];
    for fail_at in 0..=total {
        let fs = SimFs::new();
        let engine = open(&fs, options).unwrap();
        fs.fail_after(fail_at, Failure::FromThenOn);
        let run = run_until_failure(&engine, &transactions);
        drop(engine);
        for crash in crashes(seeds) {
            let crashed = fs.crash(crash);
            let engine = open(&crashed, options)
                .unwrap_or_else(|error| panic!("fail at {fail_at}, {crash:?}: {error}"));
            engine.check().unwrap_or_else(|error| {
                panic!("check after fail at {fail_at}, {crash:?}: {error}")
            });
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
            let reopened = open(&crashed.crash(Crash::DropUnsynced), options).unwrap();
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
            engine.check().unwrap_or_else(|error| {
                panic!("check after fail at {fail_at}, {crash:?}: {error}")
            });
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
            engine.check().unwrap_or_else(|error| {
                panic!("check after fail at {fail_at}, {crash:?}: {error}")
            });
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
                    .any(|name| name.ends_with(".creating"))
            );
        }
    }
}

fn commit_number(engine: &Engine<SimFs>, number: u64) -> Result<(), crate::storage::StorageError> {
    let mut write = engine.begin_write()?;
    if number > 1 {
        let earlier = number - 1 - (number * 7) % (number - 1).min(30);
        assert!(
            write
                .get(Table::NodeColumns, &earlier.to_be_bytes())?
                .is_some(),
            "commit {number} does not see commit {earlier}"
        );
    }
    write.put(
        Table::NodeColumns,
        &number.to_be_bytes(),
        &vec![number as u8; (number % 9) as usize * 400],
    )?;
    if number >= 40 {
        let old = number - 40;
        write.delete_range(
            Table::NodeColumns,
            &(old - old % 8).to_be_bytes(),
            &(old + 1).to_be_bytes(),
        )?;
    }
    write.put(Table::Metadata, b"count", &number.to_be_bytes())?;
    write.commit()
}

fn state_after(count: u64) -> State {
    let mut state = State::new();
    for number in 1..=count {
        state.insert(
            (Table::NodeColumns, number.to_be_bytes().to_vec()),
            vec![number as u8; (number % 9) as usize * 400],
        );
        if number >= 40 {
            let old = number - 40;
            let (start, end) = (
                (old - old % 8).to_be_bytes().to_vec(),
                (old + 1).to_be_bytes().to_vec(),
            );
            state.retain(|(table, key), _| {
                *table != Table::NodeColumns || *key < start || *key >= end
            });
        }
        state.insert(
            (Table::Metadata, b"count".to_vec()),
            number.to_be_bytes().to_vec(),
        );
    }
    state
}

#[test]
fn a_power_loss_at_any_moment_of_background_checkpoints_keeps_each_acknowledged_commit() {
    lose_power_at_any_moment_of_background_checkpoints(EngineOptions {
        checkpoint: CheckpointMode::Background,
        memtable_limit: 4_000,
        ..options()
    });
}

#[test]
fn a_power_loss_at_any_moment_of_commits_that_go_straight_into_the_file_keeps_each_acknowledged_commit()
 {
    lose_power_at_any_moment_of_background_checkpoints(EngineOptions {
        checkpoint: CheckpointMode::Background,
        memtable_limit: 4_000,
        ingest_limit: 2_000,
        ..options()
    });
}

fn lose_power_at_any_moment_of_background_checkpoints(background: EngineOptions) {
    let commits = u64::from(test_scale::cases(400)).min(4000);
    let fs = SimFs::new();
    let engine = open(&fs, background).unwrap();
    let acknowledged = std::sync::atomic::AtomicU64::new(0);
    let taken = std::sync::atomic::AtomicU64::new(0);
    let lead = (commits / 30).max(1);
    let mut snapshots = Vec::new();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            for number in 1..=commits {
                while number > (taken.load(std::sync::atomic::Ordering::SeqCst) + 1) * lead {
                    std::thread::yield_now();
                }
                commit_number(&engine, number).unwrap();
                acknowledged.store(number, std::sync::atomic::Ordering::SeqCst);
            }
        });
        let mut seed = 0;
        while acknowledged.load(std::sync::atomic::Ordering::SeqCst) < commits {
            let before = acknowledged.load(std::sync::atomic::Ordering::SeqCst);
            let crash = match seed % 3 {
                0 => Crash::Scramble { seed },
                1 => Crash::Reorder { seed },
                _ => Crash::DropUnsynced,
            };
            let crashed = fs.crash(crash);
            let after = acknowledged.load(std::sync::atomic::Ordering::SeqCst);
            snapshots.push((crashed, before, after, crash));
            taken.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            seed += 1;
            let next = before + (commits / 60).max(1);
            while acknowledged.load(std::sync::atomic::Ordering::SeqCst) < next.min(commits) {
                std::thread::yield_now();
            }
        }
    });
    assert!(
        engine.header().generation > 10,
        "too few background checkpoints ran"
    );
    drop(engine);
    let states: Vec<State> = (0..=commits + 1).map(state_after).collect();
    let mut checked = 0;
    for (crashed, before, after, crash) in snapshots {
        let engine = open(&crashed, options())
            .unwrap_or_else(|error| panic!("{crash:?} after {before}: {error}"));
        engine
            .check()
            .unwrap_or_else(|error| panic!("{crash:?} after {before}: {error}"));
        let found = dump(&engine);
        let recovered = found
            .get(&(Table::Metadata, b"count".to_vec()))
            .map_or(0, |bytes| {
                u64::from_be_bytes(bytes.as_slice().try_into().unwrap())
            });
        assert!(
            (before..=after + 1).contains(&recovered),
            "{crash:?}: recovered {recovered} commits, {before} to {after} were acknowledged"
        );
        assert!(
            found == states[recovered as usize],
            "{crash:?}: commit {recovered} recovered with other state"
        );
        checked += 1;
    }
    assert!(checked >= 20, "only {checked} power losses were checked");
}

#[test]
fn every_failure_then_power_loss_while_compacting_keeps_the_database_whole() {
    let transactions = transactions(13, 30);
    let states = states(&transactions);
    let fs = SimFs::new();
    let engine = open(&fs, options()).unwrap();
    for writes in &transactions {
        commit(&engine, writes).unwrap();
    }
    let crashed = fs.crash(Crash::DropUnsynced);
    drop(engine);
    let base = crashed;
    let total = {
        let copy = base.crash(Crash::DropUnsynced);
        let engine = open(&copy, options()).unwrap();
        let before = copy.operations();
        engine.compact().unwrap();
        let used = copy.operations() - before;
        assert_eq!(dump(&engine), states[transactions.len()]);
        used
    };
    assert!(total > 20, "compacting ran only {total} operations");
    for fail_at in 0..=total {
        let copy = base.crash(Crash::DropUnsynced);
        let engine = open(&copy, options()).unwrap();
        copy.fail_after(fail_at, Failure::FromThenOn);
        let _ = engine.compact();
        drop(engine);
        for crash in crashes(2) {
            let reopened = open(&copy.crash(crash), options())
                .unwrap_or_else(|error| panic!("fail at {fail_at}, {crash:?}: {error}"));
            reopened.check().unwrap_or_else(|error| {
                panic!("check after fail at {fail_at}, {crash:?}: {error}")
            });
            assert_eq!(
                dump(&reopened),
                states[transactions.len()],
                "fail at {fail_at}, {crash:?}"
            );
        }
    }
}

fn by_hand() -> EngineOptions {
    EngineOptions {
        memtable_limit: u64::MAX,
        wal_limit: u64::MAX,
        reclaim_slack: u64::MAX,
        ..options()
    }
}

fn free_blocks(engine: &Engine<SimFs>) -> BTreeMap<crate::engine::layout::ExtentId, Blocks> {
    engine
        .extents()
        .entries
        .iter()
        .map(|(id, entry)| (*id, entry.free()))
        .collect()
}

fn file_blocks(fs: &SimFs) -> u64 {
    fs.open(
        NAME,
        crate::engine::block_file::BlockSize::new(4096).unwrap(),
    )
    .unwrap()
    .block_count()
    .unwrap()
}

fn fail_each_operation_of(
    base: &SimFs,
    options: EngineOptions,
    seeds: u64,
    states: &[State],
    start: usize,
    step: &dyn Fn(&Engine<SimFs>) -> (usize, bool),
) -> usize {
    let total = {
        let copy = base.crash(Crash::KeepUnsynced);
        let engine = open(&copy, options).unwrap();
        let before = copy.operations();
        step(&engine);
        copy.operations() - before
    };
    assert!(total > 20, "the step ran only {total} operations");
    let mut staged = 0;
    for fail_at in 0..=total {
        let copy = base.crash(Crash::KeepUnsynced);
        let engine = open(&copy, options).unwrap();
        copy.fail_after(fail_at, Failure::FromThenOn);
        let (acknowledged, in_flight) = step(&engine);
        drop(engine);
        for crash in crashes(seeds) {
            let reopened = open(&copy.crash(crash), options)
                .unwrap_or_else(|error| panic!("fail at {fail_at}, {crash:?}: {error}"));
            reopened.check().unwrap_or_else(|error| {
                panic!("check after fail at {fail_at}, {crash:?}: {error}")
            });
            staged += usize::from(reopened.header().staged_lsn != 0);
            let found = dump(&reopened);
            let at = start + acknowledged;
            assert!(
                found == states[at] || (in_flight && found == states[at + 1]),
                "fail at {fail_at}, {crash:?}: {acknowledged} commits acknowledged, state differs"
            );
        }
    }
    staged
}

fn commit_then_checkpoint(engine: &Engine<SimFs>, transactions: &[Vec<Write>]) -> (usize, bool) {
    let run = run_until_failure(engine, transactions);
    if !run.in_flight {
        let _ = engine.checkpoint();
    }
    (run.acknowledged, run.in_flight)
}

#[test]
fn every_failure_then_power_loss_while_a_checkpoint_fills_freed_blocks_keeps_the_database_whole() {
    let transactions = transactions(17, 40);
    let states = states(&transactions);
    let base = SimFs::new();
    let engine = open(&base, by_hand()).unwrap();
    for chunk in transactions[..30].chunks(5) {
        for writes in chunk {
            commit(&engine, writes).unwrap();
        }
        engine.checkpoint().unwrap();
    }
    let holes = free_blocks(&engine);
    assert!(
        holes.values().map(Blocks::count).sum::<u32>() > 10,
        "the base holds no free blocks to fill"
    );
    let crashed = base.crash(Crash::DropUnsynced);
    let rest = &transactions[30..];
    commit_then_checkpoint(&engine, rest);
    let filled = engine.extents().entries.iter().any(|(id, entry)| {
        holes
            .get(id)
            .is_some_and(|free| entry.used.intersects(free))
    });
    assert!(filled, "the checkpoint placed nothing in a freed block");
    assert_eq!(dump(&engine), states[40]);
    drop(engine);
    fail_each_operation_of(&crashed, by_hand(), 2, &states, 30, &|engine| {
        commit_then_checkpoint(engine, rest)
    });
}

#[test]
fn every_failure_then_power_loss_while_a_checkpoint_cuts_the_free_tail_keeps_the_database_whole() {
    let big = |index: usize| {
        (0..2u8)
            .map(|key| {
                Write::Put(
                    Table::NodeColumns,
                    vec![index as u8, key],
                    vec![key ^ index as u8; 700_000],
                )
            })
            .collect::<Vec<_>>()
    };
    let mut transactions: Vec<Vec<Write>> = (0..6).map(big).collect();
    transactions.push(vec![Write::DeleteRange(
        Table::NodeColumns,
        vec![1],
        vec![6],
    )]);
    let states = states(&transactions);
    let base = SimFs::new();
    let unstaged = EngineOptions {
        stage_floor: u64::MAX,
        ingest_limit: u64::MAX,
        ..by_hand()
    };
    let engine = open(&base, unstaged).unwrap();
    for writes in &transactions[..6] {
        commit(&engine, writes).unwrap();
        engine.checkpoint().unwrap();
    }
    let grown = file_blocks(&base);
    let crashed = base.crash(Crash::DropUnsynced);
    let tail = &transactions[6..];
    let cut = |engine: &Engine<SimFs>| {
        let outcome = commit_then_checkpoint(engine, tail);
        if !outcome.1 {
            let _ = engine.checkpoint();
        }
        outcome
    };
    cut(&engine);
    assert_eq!(dump(&engine), states[7]);
    assert!(
        file_blocks(&base) * 2 < grown,
        "deleting the last tables left {} of {grown} blocks",
        file_blocks(&base)
    );
    drop(engine);
    fail_each_operation_of(&crashed, unstaged, 0, &states, 6, &cut);
}

#[test]
fn every_failure_then_power_loss_between_the_stages_of_a_checkpoint_keeps_the_database_whole() {
    let load: Vec<Write> = (0..60u8)
        .map(|key| Write::Put(Table::NodeColumns, vec![key], vec![key; 2000]))
        .collect();
    let rewrite: Vec<Write> = (0..60u8)
        .map(|key| {
            Write::Put(
                Table::NodeColumns,
                vec![key],
                vec![!key; 1500 + usize::from(key)],
            )
        })
        .chain([Write::DeleteRange(Table::NodeColumns, vec![50], vec![55])])
        .collect();
    let transactions = vec![load, rewrite];
    let states = states(&transactions);
    let base = SimFs::new();
    let engine = open(&base, by_hand()).unwrap();
    commit(&engine, &transactions[0]).unwrap();
    engine.checkpoint().unwrap();
    let crashed = base.crash(Crash::DropUnsynced);
    let before = engine.header().generation;
    commit_then_checkpoint(&engine, &transactions[1..]);
    assert!(
        engine.header().generation >= before + 3,
        "the rewrite was not made durable in stages"
    );
    assert_eq!(dump(&engine), states[2]);
    drop(engine);
    let staged = fail_each_operation_of(&crashed, by_hand(), 2, &states, 1, &|engine| {
        commit_then_checkpoint(engine, &transactions[1..])
    });
    assert!(staged > 0, "no power loss came back to a staged header");
}

#[test]
fn every_failure_then_power_loss_while_a_close_compacts_keeps_the_database_whole() {
    let load: Vec<Write> = (0..25u8)
        .map(|key| Write::Put(Table::NodeColumns, vec![key], vec![key; 200_000]))
        .collect();
    let keep_one = vec![Write::DeleteRange(Table::NodeColumns, vec![1], vec![30])];
    let transactions = vec![load, keep_one];
    let states = states(&transactions);
    let compacting = EngineOptions {
        reclaim_slack: 0,
        stage_floor: u64::MAX,
        ingest_limit: u64::MAX,
        ..by_hand()
    };
    let base = SimFs::new();
    let engine = open(&base, compacting).unwrap();
    commit(&engine, &transactions[0]).unwrap();
    engine.checkpoint().unwrap();
    let loaded = file_blocks(&base);
    let base = base.crash(Crash::DropUnsynced);
    drop(engine);
    let total = {
        let copy = base.crash(Crash::KeepUnsynced);
        let engine = open(&copy, compacting).unwrap();
        commit(&engine, &transactions[1]).unwrap();
        let before = copy.operations();
        drop(engine);
        assert!(
            file_blocks(&copy) * 10 < loaded,
            "closing after the delete left {} of {loaded} blocks",
            file_blocks(&copy)
        );
        let reopened = open(&copy, compacting).unwrap();
        assert_eq!(dump(&reopened), states[2]);
        copy.operations() - before
    };
    assert!(total > 20, "closing ran only {total} operations");
    for fail_at in 0..=total {
        let copy = base.crash(Crash::KeepUnsynced);
        let engine = open(&copy, compacting).unwrap();
        commit(&engine, &transactions[1]).unwrap();
        copy.fail_after(fail_at, Failure::FromThenOn);
        drop(engine);
        for crash in crashes(0) {
            let reopened = open(&copy.crash(crash), compacting)
                .unwrap_or_else(|error| panic!("fail at {fail_at}, {crash:?}: {error}"));
            reopened.check().unwrap_or_else(|error| {
                panic!("check after fail at {fail_at}, {crash:?}: {error}")
            });
            assert_eq!(dump(&reopened), states[2], "fail at {fail_at}, {crash:?}");
        }
    }
}
