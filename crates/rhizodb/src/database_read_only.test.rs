use std::collections::BTreeMap;
use std::sync::Arc;

use super::*;
use crate::engine::any_fs::AnyFs;
use crate::engine::block_file::BlockFile as _;
use crate::engine::block_file::BlockSize;
use crate::engine::block_file::FileSystem as _;
use crate::engine::session::side_name as session_file_name;
use crate::engine::sim_fs::Crash;
use crate::engine::sim_fs::Failure;
use crate::engine::sim_fs::SimFs;
use crate::engine::store::CheckpointMode;
use crate::engine::store::staging_name;
use crate::engine::wal::segment_name;
use crate::result::Value;

const NAME: &str = "graph.rhizodb";

fn options() -> EngineOptions {
    EngineOptions {
        segment_target: 8192,
        checkpoint: CheckpointMode::Inline,
        ..EngineOptions::default()
    }
}

fn open_writer(fs: &SimFs) -> Result<Database, Error> {
    let engine = Engine::open(Arc::new(AnyFs::Sim(fs.clone())), NAME, options())
        .map_err(StorageError::from)?;
    Ok(Database::from_parts(
        graph_on(engine, false)?,
        Statements::default(),
    ))
}

fn open_read_only(fs: &SimFs) -> Result<Database, Error> {
    let engine = Engine::open_read_only(Arc::new(AnyFs::Sim(fs.clone())), NAME, options())
        .map_err(StorageError::from)?;
    Ok(Database::from_parts(
        graph_on(engine, false)?,
        Statements::default(),
    ))
}

fn image(fs: &SimFs) -> [BTreeMap<String, Vec<u8>>; 2] {
    [fs.clone(), fs.crash(Crash::DropUnsynced)].map(|fs| {
        let sector = BlockSize::new(512).unwrap();
        fs.names()
            .unwrap()
            .into_iter()
            .filter(|name| *name != session_file_name(NAME))
            .map(|name| {
                let file = fs.open(&name, sector).unwrap();
                let mut bytes = vec![0; file.block_count().unwrap() as usize * 512];
                file.read(0, &mut bytes).unwrap();
                (name, bytes)
            })
            .collect()
    })
}

fn count(db: &Database, label: &str) -> i64 {
    let rows = db
        .execute(
            &format!("MATCH (n:{label}) RETURN count(n)"),
            &Parameters::new(),
        )
        .unwrap()
        .rows()
        .to_vec();
    match rows.as_slice() {
        [row] => match row.as_slice() {
            [Value::Integer(count)] => *count,
            other => panic!("count gave {other:?}"),
        },
        other => panic!("count gave {other:?}"),
    }
}

fn insert_items(db: &Database, items: i64) {
    db.create_property_index("item_i", IndexTarget::Label("Item"), "i")
        .unwrap();
    for i in 0..items {
        let parameters = Parameters::from([("i".to_owned(), Value::Integer(i))]);
        db.execute(
            "INSERT (:Item {i: $i, text: 'item ' + toString($i)})",
            &parameters,
        )
        .unwrap();
    }
}

fn kind(result: Result<impl std::fmt::Debug, Error>) -> ErrorKind {
    result.unwrap_err().kind()
}

fn assert_read_only_leaves_files_unchanged(fs: &SimFs) -> i64 {
    let before = image(fs);
    let first = open_read_only(fs).unwrap();
    let second = open_read_only(fs).unwrap();
    let items = count(&first, "Item");
    assert_eq!(count(&second, "Item"), items);
    assert_eq!(kind(open_writer(fs)), ErrorKind::DatabaseAlreadyOpen);
    let none = Parameters::new();
    assert_eq!(
        kind(first.execute("INSERT (:Item)", &none)),
        ErrorKind::ReadOnly
    );
    assert_eq!(kind(first.begin_write()), ErrorKind::ReadOnly);
    assert_eq!(kind(first.compact()), ErrorKind::ReadOnly);
    assert_eq!(kind(first.drop_index("item_i")), ErrorKind::ReadOnly);
    first.check().unwrap();
    first.stats().unwrap();
    let mut dump = Vec::new();
    second.export(&mut dump).unwrap();
    drop(second.snapshot().unwrap());
    drop((first, second));
    assert_eq!(image(fs), before, "a read-only open changed a file");
    items
}

#[test]
fn read_only_opens_after_a_clean_close_change_no_file_and_keep_out_a_writer() {
    let fs = SimFs::new();
    insert_items(&open_writer(&fs).unwrap(), 30);
    let staging = fs
        .create(&staging_name(NAME), BlockSize::new(4096).unwrap())
        .unwrap();
    staging.write(0, &[7; 4096]).unwrap();
    let leftover = fs
        .create(&segment_name(NAME, 99), BlockSize::new(512).unwrap())
        .unwrap();
    leftover.write(0, &[9; 512]).unwrap();
    fs.sync_names().unwrap();
    drop((staging, leftover));
    assert_eq!(assert_read_only_leaves_files_unchanged(&fs), 30);
    let writer = open_writer(&fs).unwrap();
    assert_eq!(kind(open_read_only(&fs)), ErrorKind::DatabaseAlreadyOpen);
    assert_eq!(count(&writer, "Item"), 30);
}

#[test]
fn read_only_opens_after_an_unclean_shutdown_replay_the_log_in_memory_and_cut_no_torn_tail() {
    let fs = SimFs::new();
    let db = open_writer(&fs).unwrap();
    insert_items(&db, 20);
    fs.fail_sync_after(0, Failure::FromThenOn);
    assert!(
        db.execute("INSERT (:Item {i: 20})", &Parameters::new())
            .is_err()
    );
    let crashes = (0..24)
        .map(|seed| Crash::Scramble { seed })
        .chain([Crash::DropUnsynced, Crash::KeepUnsynced]);
    let mut seen = std::collections::BTreeSet::new();
    for crash in crashes {
        let crashed = fs.crash(crash);
        let items = assert_read_only_leaves_files_unchanged(&crashed);
        seen.insert(items);
        let recovered = open_writer(&crashed).unwrap();
        assert_eq!(count(&recovered, "Item"), items, "{crash:?}");
    }
    assert!(seen.contains(&20), "{seen:?}");
    drop(db);
}

#[test]
fn a_read_only_open_that_fails_part_way_changes_no_file_and_the_next_one_reads() {
    let fs = SimFs::new();
    let db = open_writer(&fs).unwrap();
    insert_items(&db, 10);
    let crashed = fs.crash(Crash::KeepUnsynced);
    drop(db);
    let before = image(&crashed);
    let start = crashed.operations();
    drop(open_read_only(&crashed).unwrap());
    let operations = crashed.operations() - start;
    assert!(operations > 4, "{operations}");
    for failing in 0..operations {
        let attempt = crashed.fork();
        attempt.fail_after(failing, Failure::Once);
        if let Ok(db) = open_read_only(&attempt) {
            drop(db.execute("MATCH (n) RETURN count(n)", &Parameters::new()));
        }
        attempt.stop_failing();
        assert_eq!(image(&attempt), before, "failing operation {failing}");
    }
    assert_eq!(count(&open_read_only(&crashed).unwrap(), "Item"), 10);
}

#[test]
fn a_read_only_open_beside_a_stale_side_file_starts_a_session_over_it_and_changes_no_other_file() {
    let fs = SimFs::new();
    insert_items(&open_writer(&fs).unwrap(), 3);
    let stale = fs
        .create(&session_file_name(NAME), BlockSize::new(512).unwrap())
        .unwrap();
    stale.write(0, &[5; 512]).unwrap();
    drop(stale);
    let before = image(&fs);
    let first = open_read_only(&fs).unwrap();
    let second = open_read_only(&fs).unwrap();
    assert_eq!((count(&first, "Item"), count(&second, "Item")), (3, 3));
    assert_eq!(image(&fs), before);
}

fn open_shared_writer(fs: &SimFs) -> Result<Database, Error> {
    let engine = Engine::open_shared(Arc::new(AnyFs::Sim(fs.clone())), NAME, options())
        .map_err(StorageError::from)?;
    Ok(Database::from_parts(
        graph_on(engine, false)?,
        Statements::default(),
    ))
}

fn fives(db: &Database) -> i64 {
    let rows = db
        .execute(
            "MATCH (n:Item) WHERE n.i = 5 RETURN count(n)",
            &Parameters::new(),
        )
        .unwrap()
        .rows()
        .to_vec();
    match rows[0][..] {
        [Value::Integer(found)] => found,
        _ => panic!("expected one count, got {rows:?}"),
    }
}

#[test]
fn a_reader_that_caught_up_in_the_background_reads_with_the_writers_newest_indexes() {
    let fs = SimFs::new();
    let writer = open_shared_writer(&fs).unwrap();
    writer
        .create_property_index("item_i", IndexTarget::Label("Item"), "i")
        .unwrap();
    writer
        .execute(
            "UNWIND range(1, 10) AS i INSERT (:Item {i: i})",
            &Parameters::new(),
        )
        .unwrap();
    let reader = open_read_only(&fs).unwrap();
    assert_eq!(fives(&reader), 1);
    writer.drop_index("item_i").unwrap();
    writer
        .execute("INSERT (:Item {i: 5}), (:Item {i: 5})", &Parameters::new())
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert_eq!(
        fives(&reader),
        3,
        "the reader paired new rows with an old index"
    );
}
