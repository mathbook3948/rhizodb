use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use super::*;
use crate::result::Value;
use crate::storage::ReadTransaction as _;
use crate::storage::Storage as _;
use crate::storage::Table;

fn run(db: &Database, query: &str) -> Vec<Vec<Value>> {
    db.execute(query, &Parameters::new())
        .unwrap_or_else(|error| panic!("{query}: {error}"))
        .rows()
        .to_vec()
}

fn integer(db: &Database, query: &str) -> i64 {
    match run(db, query).as_slice() {
        [row] => match row.as_slice() {
            [Value::Integer(value)] => *value,
            other => panic!("{query} gave {other:?}"),
        },
        other => panic!("{query} gave {other:?}"),
    }
}

fn tables(db: &Database) -> Vec<(Table, Vec<crate::storage::Entry>)> {
    let read = db.graph.storage().begin_read().unwrap();
    Table::ALL
        .into_iter()
        .filter(|table| !table.is_legacy())
        .map(|table| (table, read.scan_prefix(table, &[]).unwrap()))
        .collect()
}

fn assert_consistent(db: &Database) {
    let violations = db.graph.read().unwrap().check_integrity().unwrap();
    assert!(violations.is_empty(), "{violations:?}");
}

fn populate(db: &Database) {
    db.create_property_index("item_k", IndexTarget::Label("Item"), "k")
        .unwrap();
    db.create_keyword_index("item_text", IndexTarget::Label("Item"), &["text"])
        .unwrap();
    db.create_vector_index(
        "item_vec",
        IndexTarget::Label("Item"),
        "vec",
        2,
        VectorMetric::Cosine,
    )
    .unwrap();
    db.create_keyword_index(
        "link_note",
        IndexTarget::RelationshipType("LINK"),
        &["note"],
    )
    .unwrap();
    run(
        db,
        "UNWIND range(0, 9999) AS i INSERT (:Item {k: i, text: 'word ' + toString(i % 97), \
         vec: [toFloat(i), 1.0], f: toFloat(i) / 7.0, when: date('2024-01-01') + duration({days: i})})",
    );
    run(
        db,
        "MATCH (a:Item), (b:Item) WHERE b.k = a.k + 1 AND a.k % 5 = 0 \
         INSERT (a)-[:LINK {note: 'next ' + toString(a.k)}]->(b)",
    );
    run(db, "MATCH (n:Item) WHERE n.k % 11 = 0 DETACH DELETE n");
    run(db, "MATCH (n:Item) WHERE n.k % 13 = 0 SET n:Odd REMOVE n.f");
    db.drop_index("item_vec").unwrap();
}

#[test]
fn a_backup_holds_every_table_of_the_source_and_opens_as_it() {
    let directory = tempfile::tempdir().unwrap();
    let db = Database::open(directory.path().join("source")).unwrap();
    populate(&db);
    let path = directory.path().join("copy");
    db.backup_to(&path).unwrap();
    let copy = Database::open(&path).unwrap();
    assert_eq!(tables(&copy), tables(&db));
    assert_consistent(&copy);
    assert_eq!(
        run(
            &copy,
            "CALL db.index.keyword.queryNodes('item_text', 'word', 3) YIELD node RETURN node.k"
        ),
        run(
            &db,
            "CALL db.index.keyword.queryNodes('item_text', 'word', 3) YIELD node RETURN node.k"
        ),
    );
    run(&copy, "INSERT (:Item {k: -1})");
    assert_eq!(integer(&copy, "MATCH (n:Item {k: -1}) RETURN count(n)"), 1);
    assert_eq!(integer(&db, "MATCH (n:Item {k: -1}) RETURN count(n)"), 0);
}

#[test]
fn an_in_memory_database_and_an_empty_one_back_up_to_files() {
    let directory = tempfile::tempdir().unwrap();
    let empty = Database::in_memory().unwrap();
    empty.backup_to(directory.path().join("empty")).unwrap();
    let reopened = Database::open(directory.path().join("empty")).unwrap();
    assert_eq!(tables(&reopened), tables(&empty));
    assert_eq!(integer(&reopened, "MATCH (n) RETURN count(n)"), 0);

    let db = Database::in_memory().unwrap();
    populate(&db);
    db.backup_to(directory.path().join("full")).unwrap();
    let copy = Database::open(directory.path().join("full")).unwrap();
    assert_eq!(tables(&copy), tables(&db));
    copy.backup_to(directory.path().join("again")).unwrap();
    let again = Database::open(directory.path().join("again")).unwrap();
    assert_eq!(tables(&again), tables(&db));
}

fn names_in(directory: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn a_backup_never_replaces_what_is_at_its_path() {
    let directory = tempfile::tempdir().unwrap();
    let db = Database::in_memory().unwrap();
    run(&db, "INSERT (:A)");
    let file = directory.path().join("file");
    std::fs::write(&file, b"keep me").unwrap();
    let error = db.backup_to(&file).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::BackupTargetExists);
    assert_eq!(std::fs::read(&file).unwrap(), b"keep me");
    std::fs::write(directory.path().join("empty"), b"").unwrap();
    assert_eq!(
        db.backup_to(directory.path().join("empty"))
            .unwrap_err()
            .kind(),
        ErrorKind::BackupTargetExists
    );
    std::fs::create_dir(directory.path().join("dir")).unwrap();
    assert_eq!(
        db.backup_to(directory.path().join("dir"))
            .unwrap_err()
            .kind(),
        ErrorKind::BackupTargetExists
    );
    assert_eq!(
        db.backup_to(directory.path().join("missing").join("copy"))
            .unwrap_err()
            .kind(),
        ErrorKind::Storage
    );
    assert_eq!(names_in(directory.path()), ["dir", "empty", "file"]);
}

#[test]
fn backups_taken_while_a_writer_commits_each_hold_one_commit_whole() {
    let directory = tempfile::tempdir().unwrap();
    let db = Arc::new(Database::open(directory.path().join("source")).unwrap());
    db.create_keyword_index("tick_text", IndexTarget::Label("Tick"), &["text"])
        .unwrap();
    db.create_property_index("tick_i", IndexTarget::Label("Tick"), "i")
        .unwrap();
    run(&db, "INSERT (:Counter {n: 0})");
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let db = Arc::clone(&db);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut i = 0i64;
            while !stop.load(Ordering::Relaxed) || i < 50 {
                i += 1;
                let parameters = Parameters::from([("i".to_owned(), Value::Integer(i))]);
                let mut tx = db.begin_write().unwrap();
                tx.execute(
                    "UNWIND range(1, 20) AS j INSERT (:Tick {i: $i, j: j, text: 'tick ' + toString($i)})",
                    &parameters,
                )
                .unwrap();
                tx.execute("MATCH (c:Counter) SET c.n = $i", &parameters)
                    .unwrap();
                tx.commit().unwrap();
            }
            i
        })
    };
    let mut seen = Vec::new();
    for round in 0..8 {
        let path = directory.path().join(format!("backup-{round}"));
        db.backup_to(&path).unwrap();
        let copy = Database::open(&path).unwrap();
        assert_consistent(&copy);
        let n = integer(&copy, "MATCH (c:Counter) RETURN c.n");
        assert_eq!(integer(&copy, "MATCH (t:Tick) RETURN count(t)"), n * 20);
        assert_eq!(
            integer(&copy, "MATCH (t:Tick) RETURN coalesce(max(t.i), 0)"),
            n
        );
        assert_eq!(
            integer(
                &copy,
                "CALL db.index.keyword.queryNodes('tick_text', 'tick', 100000000) YIELD node \
                 RETURN count(node)"
            ),
            n * 20
        );
        seen.push(n);
    }
    stop.store(true, Ordering::Relaxed);
    let last = writer.join().unwrap();
    assert!(seen.windows(2).all(|pair| pair[0] <= pair[1]), "{seen:?}");
    assert!(seen.iter().all(|n| *n <= last));
}

#[test]
fn a_failed_copy_leaves_no_file_behind() {
    use crate::storage::StorageError;
    use crate::storage::redb_backend::RedbRead;
    use crate::storage::redb_backend::RedbStorage;
    use crate::storage::redb_backend::RedbWrite;

    #[derive(Debug)]
    struct FailingCopy(RedbStorage);

    impl crate::storage::Storage for FailingCopy {
        type Read<'a> = RedbRead;
        type Write<'a> = RedbWrite;

        fn begin_read(&self) -> Result<RedbRead, StorageError> {
            self.0.begin_read()
        }

        fn begin_write(&self) -> Result<RedbWrite, StorageError> {
            self.0.begin_write()
        }

        fn write_copy(snapshot: &RedbRead, path: &Path) -> Result<(), StorageError> {
            RedbStorage::write_copy(snapshot, path)?;
            Err(StorageError::new(std::io::Error::other("injected")))
        }
    }

    let directory = tempfile::tempdir().unwrap();
    let storage = FailingCopy(RedbStorage::in_memory().unwrap());
    let target = directory.path().join("copy");
    assert!(crate::storage::backup::write(&storage, &target).is_err());
    assert!(names_in(directory.path()).is_empty());
}
