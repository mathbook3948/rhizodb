use std::sync::Arc;
use std::thread;
use std::time::Duration;

use super::*;
use crate::engine::layout::BLOCK;
use crate::engine::sim_fs::SimFs;
use crate::engine::store::Engine;
use crate::engine::store::EngineOptions;
use crate::storage::ReadTransaction;
use crate::storage::Storage;
use crate::storage::Table;
use crate::storage::WriteTransaction;

const NAME: &str = "graph.rhizo";

fn written(items: u8) -> SimFs {
    let fs = SimFs::new();
    let engine = Engine::open(Arc::new(fs.clone()), NAME, EngineOptions::default()).unwrap();
    let mut write = engine.begin_write().unwrap();
    for item in 0..items {
        write.put(Table::Nodes, &[item], &[item]).unwrap();
    }
    write.commit().unwrap();
    engine.close().unwrap();
    fs
}

fn reader(fs: &SimFs, build: u64) -> Result<Engine<SimFs>, EngineError> {
    let options = EngineOptions {
        build,
        ..EngineOptions::default()
    };
    Engine::open_read_only(Arc::new(fs.clone()), NAME, options)
}

fn kind(error: EngineError) -> ErrorKind {
    StorageError::from(error).kind()
}

fn items(engine: &Engine<SimFs>) -> usize {
    let read = engine.begin_read().unwrap();
    (0..=u8::MAX)
        .filter(|item| read.get(Table::Nodes, &[*item]).unwrap().is_some())
        .count()
}

#[test]
fn read_only_processes_share_a_session_each_in_a_slot_of_its_own() {
    let fs = written(3);
    let first = reader(&fs, 0).unwrap();
    let second = reader(&fs, 0).unwrap();
    assert_eq!(first.session_slot(), Some(0));
    assert_eq!(second.session_slot(), Some(1));
    assert_eq!((items(&first), items(&second)), (3, 3));
    assert!(fs.names().unwrap().contains(&side_name(NAME)));
}

#[test]
fn a_session_keeps_out_an_exclusive_open_until_its_last_process_closes() {
    let fs = written(1);
    let first = reader(&fs, 0).unwrap();
    let second = reader(&fs, 0).unwrap();
    let refused = Engine::open(Arc::new(fs.clone()), NAME, EngineOptions::default()).unwrap_err();
    assert_eq!(kind(refused), ErrorKind::DatabaseAlreadyOpen);
    drop(first);
    let refused = Engine::open(Arc::new(fs.clone()), NAME, EngineOptions::default()).unwrap_err();
    assert_eq!(kind(refused), ErrorKind::DatabaseAlreadyOpen);
    drop(second);
    Engine::open(Arc::new(fs.clone()), NAME, EngineOptions::default()).unwrap();
}

#[test]
fn a_read_only_open_waits_for_nothing_while_an_exclusive_open_holds_the_file_and_fails() {
    let fs = written(1);
    let _writer = Engine::open(Arc::new(fs.clone()), NAME, EngineOptions::default()).unwrap();
    assert_eq!(
        kind(reader(&fs, 0).unwrap_err()),
        ErrorKind::DatabaseAlreadyOpen
    );
}

#[test]
fn the_slot_of_a_closed_process_is_free_for_the_next_one() {
    let fs = written(1);
    let first = reader(&fs, 0).unwrap();
    let second = reader(&fs, 0).unwrap();
    drop(first);
    let third = reader(&fs, 0).unwrap();
    assert_eq!(third.session_slot(), Some(0));
    assert_eq!(second.session_slot(), Some(1));
}

#[test]
fn a_process_of_another_build_fails_to_join() {
    let fs = written(1);
    let _first = reader(&fs, 7).unwrap();
    let refused = reader(&fs, 8).unwrap_err();
    assert_eq!(kind(refused), ErrorKind::DatabaseAlreadyOpen);
    let _same = reader(&fs, 7).unwrap();
}

#[test]
fn a_new_session_takes_any_build_once_the_last_one_closed() {
    let fs = written(1);
    drop(reader(&fs, 7).unwrap());
    let _other = reader(&fs, 8).unwrap();
}

#[test]
fn an_open_finding_every_slot_taken_fails_with_its_own_kind() {
    let fs = written(1);
    let held: Vec<_> = (0..SLOTS).map(|_| reader(&fs, 0).unwrap()).collect();
    assert_eq!(
        kind(reader(&fs, 0).unwrap_err()),
        ErrorKind::TooManyProcesses
    );
    drop(held);
    reader(&fs, 0).unwrap();
}

#[test]
fn a_read_only_open_where_no_side_file_can_be_made_opens_alone_and_keeps_writers_out() {
    let fs = written(2);
    fs.refuse_side_files();
    let alone = reader(&fs, 0).unwrap();
    assert_eq!(alone.session_slot(), None);
    assert_eq!(items(&alone), 2);
    assert!(!fs.names().unwrap().contains(&side_name(NAME)));
    let refused = Engine::open(Arc::new(fs.clone()), NAME, EngineOptions::default()).unwrap_err();
    assert_eq!(kind(refused), ErrorKind::DatabaseAlreadyOpen);
    assert_eq!(reader(&fs, 0).unwrap().session_slot(), None);
}

#[test]
fn an_open_that_elects_itself_holds_back_every_other_until_it_finishes() {
    let fs = written(1);
    let file = fs
        .open(NAME, crate::engine::store::block_size().unwrap())
        .unwrap();
    let Joined::Session(elected) = join(&fs, NAME, &file).unwrap() else {
        panic!("a sim directory takes side files");
    };
    assert!(elected.is_first());
    let waiting = {
        let fs = fs.clone();
        thread::spawn(move || reader(&fs, 0).map(|engine| engine.session_slot()))
    };
    thread::sleep(Duration::from_millis(100));
    assert!(
        !waiting.is_finished(),
        "joined while the first process got ready"
    );
    let identity = Identity {
        build: 0,
        database_id: crate::engine::store::read_header(&file)
            .unwrap()
            .0
            .database_id,
    };
    let session = elected.finish(&file, identity, 1).unwrap();
    assert_eq!(session.slot(), 0);
    thread::sleep(Duration::from_millis(100));
    assert!(
        !waiting.is_finished(),
        "joined before the first process published"
    );
    session.publish_header(&crate::engine::store::read_header(&file).unwrap().0);
    session.ready().unwrap();
    assert_eq!(waiting.join().unwrap().unwrap(), Some(1));
}

#[test]
fn an_open_that_died_while_it_got_ready_leaves_the_next_one_first() {
    let fs = written(1);
    let file = fs
        .open(NAME, crate::engine::store::block_size().unwrap())
        .unwrap();
    let Joined::Session(elected) = join(&fs, NAME, &file).unwrap() else {
        panic!("a sim directory takes side files");
    };
    drop(elected);
    drop(file);
    let next = reader(&fs, 0).unwrap();
    assert_eq!(next.session_slot(), Some(0));
    assert_eq!(items(&next), 1);
}

#[test]
fn a_slot_is_dead_once_its_process_is_gone_and_never_while_it_lives() {
    let fs = written(1);
    let file = fs
        .open(NAME, crate::engine::store::block_size().unwrap())
        .unwrap();
    let Joined::Session(elected) = join(&fs, NAME, &file).unwrap() else {
        panic!("a sim directory takes side files");
    };
    let identity = Identity {
        build: 0,
        database_id: crate::engine::store::read_header(&file)
            .unwrap()
            .0
            .database_id,
    };
    let watcher = elected.finish(&file, identity, 4).unwrap();
    watcher.publish_header(&crate::engine::store::read_header(&file).unwrap().0);
    watcher.ready().unwrap();
    let other = reader(&fs, 0).unwrap();
    let slot = other.session_slot().unwrap();
    assert!(!watcher.slot_is_dead(slot).unwrap());
    assert!(!watcher.slot_is_dead(watcher.slot()).unwrap());
    assert_ne!(watcher.slot_generation(slot), 0);
    drop(other);
    assert!(watcher.slot_is_dead(slot).unwrap());
    assert_eq!(watcher.slot_generation(slot), 0);
    assert_eq!(watcher.slot_generation(watcher.slot()), 4);
}

fn churning() -> EngineOptions {
    EngineOptions {
        segment_target: 4096,
        memtable_limit: 2048,
        wal_limit: 16_384,
        checkpoint: crate::engine::store::CheckpointMode::Inline,
        cache_bytes: 32 * BLOCK,
        reclaim_slack: 0,
        stage_floor: 8,
        ..EngineOptions::default()
    }
}

fn writer(fs: &SimFs, options: EngineOptions) -> Result<Engine<SimFs>, EngineError> {
    Engine::open_shared(Arc::new(fs.clone()), NAME, options)
}

fn put_round(engine: &Engine<SimFs>, round: u64, keys: u64) {
    let mut write = engine.begin_write().unwrap();
    for key in 0..keys {
        write
            .put(Table::Nodes, &key.to_be_bytes(), &[round as u8; 200])
            .unwrap();
    }
    write.commit().unwrap();
}

fn round_seen(engine: &Engine<SimFs>, keys: u64) -> u8 {
    let read = engine.begin_read().unwrap();
    let values: Vec<u8> = (0..keys)
        .map(|key| read.get(Table::Nodes, &key.to_be_bytes()).unwrap().unwrap()[0])
        .collect();
    assert!(
        values.iter().all(|value| *value == values[0]),
        "a torn round: {values:?}"
    );
    values[0]
}

#[test]
fn a_reader_that_joins_a_writer_reads_every_commit_made_before_it_joined() {
    let fs = SimFs::new();
    let writer = writer(&fs, EngineOptions::default()).unwrap();
    put_round(&writer, 1, 8);
    let first = reader(&fs, 0).unwrap();
    assert_eq!(round_seen(&first, 8), 1);
    put_round(&writer, 2, 8);
    assert_eq!(round_seen(&first, 8), 1);
    assert_eq!(round_seen(&reader(&fs, 0).unwrap(), 8), 2);
}

#[test]
fn a_writer_joins_readers_and_its_commits_reach_the_readers_that_join_after() {
    let fs = written(0);
    let early = reader(&fs, 0).unwrap();
    let writer = writer(&fs, EngineOptions::default()).unwrap();
    put_round(&writer, 3, 4);
    assert_eq!(items(&early), 0);
    assert_eq!(round_seen(&reader(&fs, 0).unwrap(), 4), 3);
}

#[test]
fn a_session_takes_one_writer_and_keeps_out_an_exclusive_open() {
    let fs = SimFs::new();
    let _writer = writer(&fs, EngineOptions::default()).unwrap();
    let error = writer(&fs, EngineOptions::default()).unwrap_err();
    assert_eq!(kind(error), ErrorKind::DatabaseAlreadyOpen);
    let error = Engine::open(Arc::new(fs.clone()), NAME, EngineOptions::default()).unwrap_err();
    assert_eq!(kind(error), ErrorKind::DatabaseAlreadyOpen);
}

#[test]
fn checkpoints_reuse_no_block_that_a_reader_in_another_process_still_reads() {
    let fs = SimFs::new();
    let writer = writer(&fs, churning()).unwrap();
    put_round(&writer, 1, 64);
    let held = reader(&fs, 0).unwrap();
    for round in 2..60 {
        put_round(&writer, round, 64);
        assert_eq!(round_seen(&held, 64), 1, "after round {round}");
    }
    drop(held);
    for round in 60..80 {
        put_round(&writer, round, 64);
    }
    assert_eq!(round_seen(&reader(&fs, 0).unwrap(), 64), 79);
    writer.check().unwrap();
}

#[test]
fn a_writer_that_closes_beside_readers_leaves_its_log_to_the_next_writer() {
    let fs = SimFs::new();
    let first = writer(&fs, EngineOptions::default()).unwrap();
    put_round(&first, 1, 8);
    let held = reader(&fs, 0).unwrap();
    put_round(&first, 2, 8);
    first.close().unwrap();
    assert_eq!(round_seen(&held, 8), 1);
    let next = writer(&fs, EngineOptions::default()).unwrap();
    assert_eq!(round_seen(&next, 8), 2);
    put_round(&next, 3, 8);
    drop(held);
    next.close().unwrap();
    let alone = Engine::open(Arc::new(fs.clone()), NAME, EngineOptions::default()).unwrap();
    assert_eq!(round_seen(&alone, 8), 3);
    assert!(alone.header().clean || alone.check().is_ok());
}

#[test]
fn readers_that_join_while_a_writer_checkpoints_each_read_one_whole_round() {
    let fs = SimFs::new();
    let writer = Arc::new(writer(&fs, churning()).unwrap());
    put_round(&writer, 0, 32);
    let writing = {
        let writer = Arc::clone(&writer);
        thread::spawn(move || {
            for round in 1..120 {
                put_round(&writer, round, 32);
            }
        })
    };
    let mut seen = Vec::new();
    while !writing.is_finished() {
        let joined = reader(&fs, 0).unwrap();
        seen.push(round_seen(&joined, 32));
    }
    writing.join().unwrap();
    assert!(seen.windows(2).all(|pair| pair[0] <= pair[1]), "{seen:?}");
    assert_eq!(round_seen(&reader(&fs, 0).unwrap(), 32), 119);
}
