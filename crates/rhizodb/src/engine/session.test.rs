use std::sync::Arc;
use std::thread;
use std::time::Duration;

use super::*;
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
fn a_read_only_open_beside_a_side_file_that_cannot_be_shared_opens_alone() {
    let fs = written(1);
    fs.create(
        &side_name(NAME),
        crate::engine::store::block_size().unwrap(),
    )
    .unwrap();
    fs.refuse_side_sharing();
    assert_eq!(items(&reader(&fs, 0).unwrap()), 1);
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
