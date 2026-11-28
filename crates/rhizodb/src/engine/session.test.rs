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
        file: file.identity().unwrap(),
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
        file: file.identity().unwrap(),
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
    round_in(&engine.begin_read().unwrap(), keys)
}

fn round_in(read: &impl ReadTransaction, keys: u64) -> u8 {
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
    let before = first.begin_read().unwrap();
    assert_eq!(round_in(&before, 8), 1);
    put_round(&writer, 2, 8);
    assert_eq!(round_in(&before, 8), 1);
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
    let read = held.begin_read().unwrap();
    for round in 2..60 {
        put_round(&writer, round, 64);
        assert_eq!(round_in(&read, 64), 1, "after round {round}");
    }
    drop(read);
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
    let read = held.begin_read().unwrap();
    put_round(&first, 2, 8);
    first.close().unwrap();
    assert_eq!(round_in(&read, 8), 1);
    drop(read);
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

#[test]
fn a_reader_catches_up_with_commits_and_checkpoints_and_adopts_the_newest_tree() {
    let fs = SimFs::new();
    let writer = writer(&fs, churning()).unwrap();
    put_round(&writer, 1, 64);
    let follower = reader(&fs, 0).unwrap();
    let joined_at = follower.header().generation;
    for round in 2..40 {
        put_round(&writer, round, 64);
        follower.catch_up().unwrap();
        assert!(!follower.is_behind());
        assert_eq!(round_seen(&follower, 64), round as u8);
    }
    assert!(
        follower.header().generation > joined_at,
        "never adopted a checkpoint"
    );
    writer.check().unwrap();
    follower.check().unwrap();
}

#[test]
fn a_reader_that_fell_behind_a_released_log_adopts_the_checkpoint_that_released_it() {
    let fs = SimFs::new();
    let writer = writer(&fs, churning()).unwrap();
    put_round(&writer, 1, 64);
    let follower = reader(&fs, 0).unwrap();
    for round in 2..40 {
        put_round(&writer, round, 64);
    }
    writer.checkpoint().unwrap();
    put_round(&writer, 40, 64);
    follower.catch_up().unwrap();
    assert_eq!(round_seen(&follower, 64), 40);
}

#[test]
fn a_reader_holding_an_old_tree_across_catch_ups_keeps_reading_it_whole() {
    let fs = SimFs::new();
    let writer = writer(&fs, churning()).unwrap();
    put_round(&writer, 1, 64);
    let follower = reader(&fs, 0).unwrap();
    let held = follower.begin_read().unwrap();
    for round in 2..40 {
        put_round(&writer, round, 64);
        follower.catch_up().unwrap();
        let values: Vec<u8> = (0..64u64)
            .map(|key| held.get(Table::Nodes, &key.to_be_bytes()).unwrap().unwrap()[0])
            .collect();
        assert!(
            values.iter().all(|value| *value == 1),
            "round {round}: {values:?}"
        );
    }
    drop(held);
    put_round(&writer, 40, 64);
    follower.catch_up().unwrap();
    assert_eq!(round_seen(&follower, 64), 40);
}

#[test]
fn a_reader_catching_up_while_a_writer_checkpoints_reads_whole_rounds_that_never_go_back() {
    let fs = SimFs::new();
    let writer = Arc::new(writer(&fs, churning()).unwrap());
    put_round(&writer, 0, 32);
    let follower = reader(&fs, 0).unwrap();
    let writing = {
        let writer = Arc::clone(&writer);
        thread::spawn(move || {
            for round in 1..150 {
                put_round(&writer, round, 32);
            }
        })
    };
    let mut last = 0;
    let mut held = Vec::new();
    while !writing.is_finished() {
        follower.catch_up().unwrap();
        let seen = round_seen(&follower, 32);
        assert!(seen >= last, "went back from {last} to {seen}");
        last = seen;
        held.push(follower.begin_read().unwrap());
        if held.len() > 4 {
            held.remove(0);
        }
    }
    writing.join().unwrap();
    drop(held);
    follower.catch_up().unwrap();
    assert_eq!(round_seen(&follower, 32), 149);
    writer.check().unwrap();
}

#[test]
fn a_read_only_open_where_side_files_cannot_be_shared_opens_alone_and_leaves_none() {
    let fs = written(2);
    fs.refuse_side_sharing();
    let first = reader(&fs, 0).unwrap();
    assert_eq!(first.session_slot(), None);
    assert!(
        !fs.names().unwrap().contains(&side_name(NAME)),
        "left a side file"
    );
    let second = reader(&fs, 0).unwrap();
    assert_eq!((items(&first), items(&second)), (2, 2));
}

#[test]
fn a_read_only_open_beside_a_side_file_that_cannot_be_shared_refuses_the_file() {
    let fs = written(1);
    fs.create(
        &side_name(NAME),
        crate::engine::store::block_size().unwrap(),
    )
    .unwrap();
    fs.refuse_side_sharing();
    assert_eq!(
        kind(reader(&fs, 0).unwrap_err()),
        ErrorKind::DatabaseAlreadyOpen
    );
}

#[test]
fn the_last_process_of_a_session_removes_the_side_file() {
    let fs = written(1);
    let first = reader(&fs, 0).unwrap();
    let second = reader(&fs, 0).unwrap();
    drop(first);
    assert!(fs.names().unwrap().contains(&side_name(NAME)));
    drop(second);
    assert!(
        !fs.names().unwrap().contains(&side_name(NAME)),
        "the side file stayed"
    );
    assert_eq!(reader(&fs, 0).unwrap().session_slot(), Some(0));
}

#[test]
fn an_open_holding_a_side_file_the_last_process_removed_opens_the_new_one() {
    let fs = written(1);
    let early = fs.open_side(&side_name(NAME));
    assert!(early.is_err(), "no side file before any open");
    let first = reader(&fs, 0).unwrap();
    let stale = fs.open_side(&side_name(NAME)).unwrap();
    drop(first);
    assert!(!stale.is_named().unwrap());
    let next = reader(&fs, 0).unwrap();
    let held = fs.open_side(&side_name(NAME)).unwrap();
    assert!(held.is_named().unwrap());
    assert_eq!(next.session_slot(), Some(0));
}

#[test]
fn a_process_whose_database_file_differs_fails_to_join() {
    let fs = written(1);
    let file = fs
        .open(NAME, crate::engine::store::block_size().unwrap())
        .unwrap();
    let _first = reader(&fs, 0).unwrap();
    let Joined::Session(elected) = join(&fs, NAME, &file).unwrap() else {
        panic!("a sim directory takes side files");
    };
    let identity = Identity {
        build: 0,
        database_id: crate::engine::store::read_header(&file)
            .unwrap()
            .0
            .database_id,
        file: (0, u128::MAX),
    };
    let error = elected.finish(&file, identity, 1).unwrap_err();
    assert_eq!(kind(error), ErrorKind::DatabaseAlreadyOpen);
}

#[test]
fn a_writer_that_takes_over_reuses_no_block_a_reader_of_the_writer_before_it_reads() {
    let fs = SimFs::new();
    let first = writer(&fs, churning()).unwrap();
    put_round(&first, 1, 1024);
    first.checkpoint().unwrap();
    let options = EngineOptions {
        cache_bytes: 4 * BLOCK,
        ..EngineOptions::default()
    };
    let held = Engine::open_read_only(Arc::new(fs.clone()), NAME, options).unwrap();
    let read = held.begin_read().unwrap();
    for round in 2..30 {
        put_round(&first, round, 1024);
    }
    first.close().unwrap();
    let second = writer(&fs, churning()).unwrap();
    for round in 30..90 {
        put_round(&second, round, 1024);
        if round % 10 == 0 {
            second.compact().unwrap();
        }
    }
    assert_eq!(round_in(&read, 1024), 1);
    drop(read);
    held.check().unwrap();
}

fn put_odd(engine: &Engine<SimFs>, round: u64, keys: u64) {
    let mut write = engine.begin_write().unwrap();
    for key in (1..keys).step_by(2) {
        write
            .put(Table::Nodes, &key.to_be_bytes(), &[round as u8; 200])
            .unwrap();
    }
    write.commit().unwrap();
}

fn evens_from_the_tree(read: &impl ReadTransaction, keys: u64) {
    for key in (0..keys).step_by(2) {
        let value = read.get(Table::Nodes, &key.to_be_bytes()).unwrap().unwrap();
        assert_eq!(value[0], 1, "key {key} lost the value only the tree holds");
    }
}

#[test]
fn a_reader_that_joins_while_a_stage_chose_its_space_reads_its_tree_whole() {
    let fs = SimFs::new();
    let writer = writer(&fs, churning()).unwrap();
    put_round(&writer, 1, 1024);
    writer.checkpoint().unwrap();
    put_odd(&writer, 2, 1024);
    writer.checkpoint().unwrap();
    let joined = std::rc::Rc::new(std::cell::RefCell::new(None));
    {
        let joined = std::rc::Rc::clone(&joined);
        let fs = fs.clone();
        crate::engine::store::STAGE_PLACED.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                let engine = Box::leak(Box::new(reader(&fs, 0).unwrap()));
                *joined.borrow_mut() = Some(engine.begin_read().unwrap());
            }));
        });
    }
    for round in 3..60 {
        put_odd(&writer, round, 1024);
    }
    let joined = joined
        .borrow_mut()
        .take()
        .expect("no checkpoint ran in stages");
    writer.compact().unwrap();
    evens_from_the_tree(&joined, 1024);
}

#[test]
fn a_fallback_open_refuses_a_file_that_a_session_holds_elsewhere() {
    let fs = written(1);
    let _member = reader(&fs, 0).unwrap();
    fs.refuse_side_sharing();
    assert_eq!(
        kind(reader(&fs, 0).unwrap_err()),
        ErrorKind::DatabaseAlreadyOpen
    );
}

#[test]
fn no_second_session_starts_over_a_file_that_a_session_through_another_name_holds() {
    let fs = written(1);
    let _member = reader(&fs, 0).unwrap();
    let file = fs
        .open(NAME, crate::engine::store::block_size().unwrap())
        .unwrap();
    fs.rename(&side_name(NAME), "elsewhere-shm").unwrap();
    match join(&fs, NAME, &file) {
        Err(error) => assert_eq!(kind(error), ErrorKind::DatabaseAlreadyOpen),
        Ok(Joined::Alone { .. }) => {}
        Ok(Joined::Session(_)) => panic!("a second session started over one file"),
    }
}

fn file_blocks(fs: &SimFs) -> u64 {
    fs.open(NAME, crate::engine::store::block_size().unwrap())
        .unwrap()
        .block_count()
        .unwrap()
}

fn churn_in_batches(fs: &SimFs, writer: &Engine<SimFs>) {
    for batch in 0..10u64 {
        for round in 0..20 {
            put_round(writer, batch * 20 + round, 256);
        }
        thread::sleep(Duration::from_millis(250));
    }
    writer.compact().unwrap();
    let _ = fs;
}

#[test]
fn an_idle_reader_adopts_checkpoints_on_its_own_so_the_file_stays_bounded() {
    let alone = SimFs::new();
    let writer_alone = writer(&alone, churning()).unwrap();
    churn_in_batches(&alone, &writer_alone);
    let baseline = file_blocks(&alone);
    let fs = SimFs::new();
    let busy = writer(&fs, churning()).unwrap();
    put_round(&busy, 0, 256);
    let idle = reader(&fs, 0).unwrap();
    let joined_at = idle.header().generation;
    churn_in_batches(&fs, &busy);
    let grown = file_blocks(&fs);
    assert!(
        idle.header().generation > joined_at,
        "the idle reader never adopted a checkpoint"
    );
    assert!(
        grown <= baseline * 2 + 2048,
        "an idle reader grew the file to {grown} blocks against {baseline} alone"
    );
}

#[test]
fn adopting_a_checkpoint_keeps_every_commit_after_it_and_needs_no_released_log() {
    let fs = SimFs::new();
    let writer = writer(&fs, EngineOptions::default()).unwrap();
    put_round(&writer, 1, 16);
    let follower = reader(&fs, 0).unwrap();
    let joined_at = follower.header().generation;
    writer.checkpoint_keeping_log().unwrap();
    put_round(&writer, 2, 16);
    follower.catch_up().unwrap();
    assert!(
        follower.header().generation > joined_at,
        "a caught-up reader did not adopt a checkpoint whose log is still there"
    );
    assert_eq!(
        round_seen(&follower, 16),
        2,
        "the commit after the checkpoint is gone"
    );
}

#[test]
fn a_reader_adopting_while_a_checkpoint_runs_in_stages_reads_one_whole_round() {
    let fs = SimFs::new();
    let writer = writer(&fs, churning()).unwrap();
    put_round(&writer, 1, 1024);
    writer.checkpoint().unwrap();
    let follower = std::rc::Rc::new(reader(&fs, 0).unwrap());
    for round in 2..30 {
        put_round(&writer, round, 1024);
    }
    let ran = std::rc::Rc::new(std::cell::Cell::new(false));
    {
        let follower = std::rc::Rc::clone(&follower);
        let ran = std::rc::Rc::clone(&ran);
        crate::engine::store::STAGE_PLACED.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                let during = std::rc::Rc::clone(&follower);
                crate::engine::store::ADOPTED.with(|hook| {
                    *hook.borrow_mut() = Some(Box::new(move || {
                        round_seen(&during, 1024);
                    }));
                });
                follower.catch_up().unwrap();
                ran.set(true);
            }));
        });
    }
    for round in 30..60 {
        if ran.get() {
            break;
        }
        put_round(&writer, round, 1024);
    }
    assert!(ran.get(), "no checkpoint ran in stages");
    round_seen(&follower, 1024);
}

fn put_big(engine: &Engine<SimFs>, round: u8, keys: std::ops::Range<u64>) {
    let mut write = engine.begin_write().unwrap();
    for key in keys {
        write
            .put(Table::Nodes, &key.to_be_bytes(), &vec![round; 3000])
            .unwrap();
    }
    write.commit().unwrap();
}

fn delete_from(engine: &Engine<SimFs>, keys: std::ops::Range<u64>) {
    let mut write = engine.begin_write().unwrap();
    write
        .delete_range(
            Table::Nodes,
            &keys.start.to_be_bytes(),
            &keys.end.to_be_bytes(),
        )
        .unwrap();
    write.commit().unwrap();
}

#[test]
fn a_writer_that_takes_over_writes_no_extent_into_the_trailing_slots_a_reader_reaches() {
    let fs = SimFs::new();
    let first = writer(&fs, EngineOptions::default()).unwrap();
    put_big(&first, 1, 0..6000);
    put_big(&first, 1, 10_000..16_000);
    first.checkpoint().unwrap();
    delete_from(&first, 0..6000);
    first.checkpoint().unwrap();
    let held = reader(&fs, 0).unwrap();
    let read = held.begin_read().unwrap();
    delete_from(&first, 10_000..16_000);
    first.checkpoint().unwrap();
    first.close().unwrap();
    let second = writer(&fs, EngineOptions::default()).unwrap();
    put_big(&second, 2, 20_000..32_000);
    second.checkpoint().unwrap();
    for key in (10_000..16_000u64).step_by(7) {
        let value = read.get(Table::Nodes, &key.to_be_bytes()).unwrap().unwrap();
        assert_eq!(value[0], 1, "key {key}");
    }
}

#[test]
fn a_reader_takes_a_log_segment_the_writer_created_but_has_not_written_for_one_still_to_come() {
    use crate::engine::block_file::BlockSize;
    use crate::engine::block_file::FileSystem as _;
    use crate::engine::wal::segment_files;
    use crate::engine::wal::segment_name;
    let fs = SimFs::new();
    let writer = writer(&fs, EngineOptions::default()).unwrap();
    put_round(&writer, 0, 32);
    let follower = reader(&fs, 0).unwrap();
    for round in 1..6 {
        put_round(&writer, round, 32);
    }
    let next = segment_files(&fs, NAME).unwrap().into_iter().max().unwrap() + 1;
    let empty = segment_name(NAME, next);
    drop(fs.create(&empty, BlockSize::new(4096).unwrap()).unwrap());
    let joined = reader(&fs, 0).unwrap();
    assert_eq!(round_seen(&joined, 32), 5);
    follower.catch_up().unwrap();
    assert_eq!(round_seen(&follower, 32), 5);
    fs.remove(&empty).unwrap();
    put_round(&writer, 6, 32);
    follower.catch_up().unwrap();
    assert_eq!(round_seen(&follower, 32), 6);
}

#[test]
fn a_first_process_that_fails_before_it_is_ready_leaves_no_side_file_and_no_lock() {
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
        file: file.identity().unwrap(),
    };
    let session = elected.finish(&file, identity, 1).unwrap();
    drop(session);
    drop(file);
    assert!(
        !fs.names().unwrap().contains(&side_name(NAME)),
        "the side file outlived the only process of its session"
    );
    let next = reader(&fs, 0).unwrap();
    assert_eq!(next.session_slot(), Some(0));
    assert_eq!(items(&next), 1);
}

#[test]
fn a_reader_reports_a_torn_log_segment_it_needs_as_corrupt_at_once() {
    use crate::engine::block_file::BlockFile as _;
    use crate::engine::block_file::BlockSize;
    use crate::engine::block_file::FileSystem as _;
    use crate::engine::wal::segment_files;
    use crate::engine::wal::segment_name;
    let fs = SimFs::new();
    let options = EngineOptions {
        segment_target: 4096,
        ..EngineOptions::default()
    };
    let writer = writer(&fs, options).unwrap();
    put_round(&writer, 0, 32);
    let follower = reader(&fs, 0).unwrap();
    for round in 1..6 {
        put_round(&writer, round, 32);
    }
    let segments = segment_files(&fs, NAME).unwrap();
    assert!(segments.len() > 2, "the log rolled over");
    let newest = segment_name(NAME, *segments.last().unwrap());
    let size = BlockSize::new(4096).unwrap();
    fs.open(&newest, size)
        .unwrap()
        .write(0, &[0; 4096])
        .unwrap();
    let started = std::time::Instant::now();
    let error = follower.catch_up().unwrap_err();
    let message = error.to_string();
    assert!(message.contains("log segment"), "{message}");
    assert_eq!(kind(error), ErrorKind::CorruptData, "{message}");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "the reader retried a torn segment it needs"
    );
}
