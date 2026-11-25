mod support;

use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use rhizodb::Database;
use rhizodb::ErrorKind;
use rhizodb::Parameters;
use rhizodb::Value;
use support::*;

#[test]
fn maintenance_fails_on_the_thread_that_holds_the_write_transaction() {
    let db = db();
    let write = db.begin_write().unwrap();
    assert_eq!(
        db.compact().unwrap_err().kind(),
        ErrorKind::WriteTransactionOnThisThread
    );
    assert_eq!(
        db.check().unwrap_err().kind(),
        ErrorKind::WriteTransactionOnThisThread
    );
    drop(write);
}

fn count(db: &Database) -> Value {
    single(db, "MATCH (n) RETURN count(*)")
}

fn fill(db: &Database) {
    run(
        db,
        "UNWIND range(1, 20000) AS i CREATE (:Item {i: i, text: 'item number ' + toString(i), pad: 'x' + toString(i * 7919)})",
    );
}

fn fill_with_long_values(db: &Database) {
    let long = Value::String("v".repeat(3000));
    db.execute(
        "UNWIND range(1, 4000) AS i CREATE (:Doc {i: i, body: $long + toString(i)})",
        &Parameters::from([("long".to_owned(), long)]),
    )
    .unwrap();
}

#[test]
fn compacting_gives_back_the_space_of_deleted_values_and_keeps_every_node() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph.db");
    let db = Database::open(&path).unwrap();
    fill_with_long_values(&db);
    db.compact().unwrap();
    let full = db.stats().unwrap();
    run(&db, "MATCH (n:Doc) WHERE n.i % 100 <> 0 DETACH DELETE n");
    db.compact().unwrap();
    let compacted = db.stats().unwrap();
    assert!(
        compacted.file_bytes * 2 < full.file_bytes,
        "{full:?} then {compacted:?}"
    );
    assert!(
        compacted.live_bytes * 10 < full.live_bytes,
        "{full:?} then {compacted:?}"
    );
    assert!(
        compacted.live_extents < full.live_extents,
        "{full:?} then {compacted:?}"
    );
    assert_eq!(compacted.free_extents, 0, "{compacted:?}");
    assert_eq!(count(&db), Value::Integer(40));
    assert_eq!(
        single(&db, "MATCH (n:Doc {i: 300}) RETURN size(n.body)"),
        Value::Integer(3003)
    );
    db.check().unwrap();
    drop(db);
    let reopened = Database::open(&path).unwrap();
    reopened.check().unwrap();
    assert_eq!(count(&reopened), Value::Integer(40));
    assert_eq!(
        std::fs::metadata(&path).unwrap().len(),
        compacted.file_bytes
    );
}

#[test]
fn closing_after_deleting_nearly_everything_gives_the_space_back_and_keeps_the_rest() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph.db");
    {
        let db = Database::open(&path).unwrap();
        fill_with_long_values(&db);
    }
    let loaded = std::fs::metadata(&path).unwrap().len();
    {
        let db = Database::open(&path).unwrap();
        run(&db, "MATCH (n:Doc) WHERE n.i % 100 <> 0 DETACH DELETE n");
    }
    let closed = std::fs::metadata(&path).unwrap().len();
    assert!(
        closed * 4 < loaded,
        "the file kept {closed} of {loaded} bytes after the deletes"
    );
    let db = Database::open(&path).unwrap();
    db.check().unwrap();
    assert_eq!(count(&db), Value::Integer(40));
    assert_eq!(
        single(&db, "MATCH (n:Doc {i: 300}) RETURN size(n.body)"),
        Value::Integer(3003)
    );
}

#[test]
fn stats_count_the_log_and_the_cache() {
    let directory = tempfile::tempdir().unwrap();
    let db = Database::open(directory.path().join("graph.db")).unwrap();
    run(&db, "CREATE (:Item {i: 1})");
    let written = db.stats().unwrap();
    assert!(written.wal_bytes > 0, "{written:?}");
    fill(&db);
    db.compact().unwrap();
    let compacted = db.stats().unwrap();
    assert!(compacted.live_extents > 0, "{compacted:?}");
    assert!(compacted.file_bytes > 0, "{compacted:?}");
    for _ in 0..3 {
        assert_eq!(count(&db), Value::Integer(20001));
    }
    let read = db.stats().unwrap();
    assert!(read.cache_hits > compacted.cache_hits, "{read:?}");
}

#[test]
fn reads_on_other_threads_see_every_node_while_compactions_run() {
    let db = db();
    fill(&db);
    run(&db, "MATCH (n:Item) WHERE n.i % 2 = 0 DETACH DELETE n");
    let done = AtomicBool::new(false);
    std::thread::scope(|scope| {
        for _ in 0..3 {
            scope.spawn(|| {
                while !done.load(Ordering::Relaxed) {
                    assert_eq!(count(&db), Value::Integer(10000));
                    assert_eq!(
                        single(&db, "MATCH (n:Item {i: 9999}) RETURN n.text"),
                        Value::String("item number 9999".to_owned())
                    );
                }
            });
        }
        for _ in 0..4 {
            db.compact().unwrap();
            db.check().unwrap();
        }
        done.store(true, Ordering::Relaxed);
    });
}

#[test]
fn a_writer_on_another_thread_waits_for_a_compaction_and_both_land() {
    let db = db();
    fill(&db);
    std::thread::scope(|scope| {
        let writer = scope.spawn(|| {
            for i in 0..50 {
                run(&db, &format!("CREATE (:Late {{i: {i}}})"));
            }
        });
        for _ in 0..3 {
            db.compact().unwrap();
        }
        writer.join().unwrap();
    });
    db.check().unwrap();
    assert_eq!(count(&db), Value::Integer(20050));
}

#[test]
fn a_check_finds_a_damaged_page_that_no_query_read() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph.db");
    {
        let db = Database::open(&path).unwrap();
        fill(&db);
        db.compact().unwrap();
    }
    let pristine = std::fs::read(&path).unwrap();
    let written: Vec<usize> = pristine
        .chunks(4096)
        .enumerate()
        .skip(2)
        .filter(|(_, block)| block.iter().any(|byte| *byte != 0))
        .map(|(block, _)| block)
        .collect();
    assert!(written.len() > 8, "{} blocks written", written.len());
    let step = written.len().div_ceil(24);
    let mut refused = 0;
    for block in written.iter().step_by(step) {
        let copy = directory.path().join(format!("damaged-{block}.db"));
        let mut bytes = pristine.clone();
        bytes[block * 4096 + 100] ^= 0x40;
        std::fs::write(&copy, &bytes).unwrap();
        match Database::open(&copy) {
            Err(error) => assert_eq!(error.kind(), ErrorKind::CorruptData, "block {block}"),
            Ok(db) => {
                assert_eq!(
                    db.check().unwrap_err().kind(),
                    ErrorKind::CorruptData,
                    "block {block}"
                );
                refused += 1;
            }
        }
    }
    assert!(refused > 0);
}
