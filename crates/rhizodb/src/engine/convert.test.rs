use std::collections::BTreeMap;
use std::sync::Arc;

use super::*;
use crate::engine::block_file::FileSystem;
use crate::engine::sim_fs::Crash;
use crate::engine::sim_fs::Failure;
use crate::engine::sim_fs::SimFs;
use crate::engine::store::CheckpointMode;
use crate::storage::redb_backend::BlockBackend;
use crate::storage::redb_backend::RedbStorage;
use crate::test_scale;

const NAME: &str = "graph.rhizo";

type State = BTreeMap<(Table, Vec<u8>), Vec<u8>>;

fn options() -> EngineOptions {
    EngineOptions {
        segment_target: 8192,
        memtable_limit: 6_000,
        wal_limit: 20_000,
        checkpoint: CheckpointMode::Inline,
        cache_pages: 64,
    }
}

fn state() -> State {
    let mut state = State::new();
    for index in 0u32..60 {
        let table =
            [Table::Metadata, Table::NodeColumns, Table::PropertyEntries][index as usize % 3];
        let key = index.to_be_bytes().to_vec();
        let length = match index {
            5 => 70_000,
            _ if index % 17 == 0 => 3000,
            _ => (index % 40) as usize,
        };
        state.insert(
            (table, key),
            (0..length).map(|at| (at as u32 + index) as u8).collect(),
        );
    }
    state.insert((Table::Outgoing, vec![7; 1500]), b"a long key".to_vec());
    state.insert(
        (Table::Nodes, b"legacy".to_vec()),
        b"a table only older formats hold".to_vec(),
    );
    state
}

fn redb_file(fs: &SimFs, state: &State) {
    let file = fs.create(NAME, sector_size().unwrap()).unwrap();
    fs.sync_names().unwrap();
    let redb = RedbStorage::with_backend(BlockBackend(Arc::new(file))).unwrap();
    let entries: Vec<_> = state.iter().collect();
    for chunk in entries.chunks(25) {
        let mut write = redb.begin_write().unwrap();
        for ((table, key), value) in chunk {
            write.put(*table, key, value).unwrap();
        }
        write.commit().unwrap();
    }
}

fn file_bytes(fs: &SimFs, name: &str) -> Vec<u8> {
    let file = fs.open(name, sector_size().unwrap()).unwrap();
    let mut bytes = vec![0; file.block_count().unwrap() as usize * SECTOR_SIZE];
    file.read(0, &mut bytes).unwrap();
    bytes
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

fn open(fs: &SimFs) -> Result<Engine<SimFs>, EngineError> {
    Engine::open(Arc::new(fs.clone()), NAME, options())
}

fn assert_no_leftovers(fs: &SimFs) {
    let names = fs.names().unwrap();
    assert!(
        names.iter().all(|name| !name.starts_with('.')),
        "leftover files: {names:?}"
    );
}

fn redb_database() -> (SimFs, State) {
    let fs = SimFs::new();
    let state = state();
    redb_file(&fs, &state);
    (fs.crash(Crash::DropUnsynced), state)
}

#[test]
fn opening_a_redb_file_converts_every_table_into_the_engine() {
    let (fs, state) = redb_database();
    let engine = open(&fs).unwrap();
    assert_eq!(dump(&engine), state);
    engine.check().unwrap();
    assert!(!holds_redb(&fs.open(NAME, sector_size().unwrap()).unwrap()).unwrap());
    assert_no_leftovers(&fs);
    drop(engine);
    assert_eq!(dump(&open(&fs.crash(Crash::DropUnsynced)).unwrap()), state);
}

fn fail_each_operation_of_a_conversion(base: &SimFs, state: &State, shuffles: bool) {
    let original = file_bytes(base, NAME);
    let total = {
        let copy = base.crash(Crash::DropUnsynced);
        let before = copy.operations();
        drop(open(&copy).unwrap());
        copy.operations() - before
    };
    assert!(total > 50, "the conversion ran only {total} operations");
    let seeds = u64::from(test_scale::cases(1));
    let mut outcomes = [0usize; 2];
    for fail_at in 0..=total {
        let copy = base.crash(Crash::DropUnsynced);
        copy.fail_after(fail_at, Failure::FromThenOn);
        drop(open(&copy));
        copy.stop_failing();
        let mut crashes = vec![Crash::DropUnsynced, Crash::KeepUnsynced];
        if shuffles {
            let seed = fail_at / 2 % seeds;
            crashes.push(if fail_at % 2 == 0 {
                Crash::Scramble { seed }
            } else {
                Crash::Reorder { seed }
            });
        }
        for crash in &crashes {
            let crashed = copy.crash(*crash);
            let file = crashed.open(NAME, sector_size().unwrap()).unwrap();
            let still_redb = holds_redb(&file).unwrap();
            if still_redb {
                assert!(
                    file_bytes(&crashed, NAME) == original,
                    "fail at {fail_at}, {crash:?}: the redb file changed before the rename"
                );
            }
            outcomes[usize::from(still_redb)] += 1;
            let engine = open(&crashed)
                .unwrap_or_else(|error| panic!("fail at {fail_at}, {crash:?}: {error}"));
            assert!(
                dump(&engine) == *state,
                "fail at {fail_at}, {crash:?}: state differs"
            );
            engine.check().unwrap_or_else(|error| {
                panic!("check after fail at {fail_at}, {crash:?}: {error}")
            });
            assert_no_leftovers(&crashed);
        }
    }
    assert!(outcomes[0] > 0 && outcomes[1] > 0, "{outcomes:?}");
}

#[test]
fn every_failure_then_power_loss_during_a_conversion_leaves_the_redb_file_or_the_whole_copy() {
    let (base, state) = redb_database();
    fail_each_operation_of_a_conversion(&base, &state, true);
}

fn unclean_redb_database() -> (SimFs, State) {
    let fs = SimFs::new();
    let state = state();
    let file = fs.create(NAME, sector_size().unwrap()).unwrap();
    fs.sync_names().unwrap();
    let redb = RedbStorage::with_backend(BlockBackend(Arc::new(file))).unwrap();
    for ((table, key), value) in &state {
        let mut write = redb.begin_write().unwrap();
        write.put(*table, key, value).unwrap();
        write.commit().unwrap();
    }
    let crashed = fs.crash(Crash::DropUnsynced);
    drop(redb);
    (crashed, state)
}

#[test]
fn a_redb_file_left_by_a_crash_converts_after_redb_recovers_it_in_memory() {
    let (base, state) = unclean_redb_database();
    let engine = open(&base.crash(Crash::DropUnsynced)).unwrap();
    assert_eq!(dump(&engine), state);
    engine.check().unwrap();
    drop(engine);
    fail_each_operation_of_a_conversion(&base, &state, false);
}

#[test]
fn a_damaged_redb_file_fails_to_convert_and_stays_as_it_was() {
    let (base, state) = redb_database();
    let original = file_bytes(&base, NAME);
    let mut refused = 0;
    let step = original.len() / test_scale::cases(40) as usize;
    let header_damage = [9, 12, 20, 40];
    for (index, offset) in header_damage
        .into_iter()
        .chain((step / 2..original.len()).step_by(step))
        .enumerate()
    {
        let fs = base.crash(Crash::DropUnsynced);
        let file = fs.open(NAME, sector_size().unwrap()).unwrap();
        let sector = offset / SECTOR_SIZE;
        let mut bytes = vec![0; SECTOR_SIZE];
        file.read(sector as u64, &mut bytes).unwrap();
        bytes[offset % SECTOR_SIZE] ^= 1 << (index % 8);
        file.write(sector as u64, &bytes).unwrap();
        file.sync().unwrap();
        let damaged = file_bytes(&fs, NAME);
        drop(file);
        match open(&fs) {
            Ok(engine) => {
                engine.check().unwrap();
                let found = dump(&engine);
                assert!(found.len() <= state.len() + 1, "offset {offset}");
            }
            Err(_) => {
                refused += 1;
                assert!(
                    file_bytes(&fs, NAME) == damaged,
                    "offset {offset}: the damaged file changed"
                );
                assert!(open(&fs).is_err(), "offset {offset}: a second open passed");
            }
        }
    }
    assert!(refused > 0);
}

#[test]
fn a_file_that_is_neither_redb_nor_the_engine_is_refused_unchanged() {
    let fs = SimFs::new();
    let file = fs.create(NAME, sector_size().unwrap()).unwrap();
    fs.sync_names().unwrap();
    let mut bytes = vec![7; 8 * SECTOR_SIZE];
    bytes[..4].copy_from_slice(b"redb");
    file.write(0, &bytes).unwrap();
    file.sync().unwrap();
    drop(file);
    assert!(open(&fs).is_err());
    assert_eq!(file_bytes(&fs, NAME), bytes);
}

#[test]
fn opens_racing_on_threads_convert_a_redb_file_once_and_lose_nothing() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join(NAME);
    let state = state();
    {
        let redb = RedbStorage::open(&path).unwrap();
        let mut write = redb.begin_write().unwrap();
        for ((table, key), value) in &state {
            write.put(*table, key, value).unwrap();
        }
        write.commit().unwrap();
    }
    let opened = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..6 {
            scope.spawn(|| {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
                loop {
                    match crate::engine::any_fs::open_path(&path, EngineOptions::default()) {
                        Ok(engine) => {
                            assert_eq!(dump(&engine), state);
                            opened.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            return;
                        }
                        Err(EngineError::Io(error))
                            if error.kind() == io::ErrorKind::WouldBlock =>
                        {
                            assert!(
                                std::time::Instant::now() < deadline,
                                "an open waited too long"
                            );
                            std::thread::yield_now();
                        }
                        Err(error) => panic!("{error}"),
                    }
                }
            });
        }
    });
    assert_eq!(opened.into_inner(), 6);
    let names: Vec<String> = std::fs::read_dir(directory.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(names.iter().all(|name| !name.starts_with('.')), "{names:?}");
    assert!(!std::fs::read(&path).unwrap().starts_with(&REDB_MAGIC));
}

#[test]
fn a_redb_file_that_redb_holds_open_is_not_converted() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join(NAME);
    let redb = RedbStorage::open(&path).unwrap();
    let mut write = redb.begin_write().unwrap();
    write.put(Table::Metadata, b"k", b"v").unwrap();
    write.commit().unwrap();
    let refused = crate::engine::any_fs::open_path(&path, EngineOptions::default());
    assert!(
        matches!(&refused, Err(EngineError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock),
        "{refused:?}"
    );
    drop(redb);
    let engine = crate::engine::any_fs::open_path(&path, EngineOptions::default()).unwrap();
    assert_eq!(
        engine
            .begin_read()
            .unwrap()
            .get(Table::Metadata, b"k")
            .unwrap(),
        Some(b"v".to_vec())
    );
}
