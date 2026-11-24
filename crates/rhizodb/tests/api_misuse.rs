mod support;

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::OnceLock;
use std::thread;

use rhizodb::Database;
use rhizodb::ErrorKind;
use rhizodb::IndexTarget;
use rhizodb::Parameters;
use rhizodb::Procedure;
use rhizodb::Value;
use rhizodb::VectorMetric;
use support::*;

fn graph_file() -> (tempfile::TempDir, std::path::PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph.redb");
    (directory, path)
}

#[test]
fn opening_a_file_that_is_already_open_fails_and_leaves_the_open_database_working() {
    let (_directory, path) = graph_file();
    let first = Database::open(&path).unwrap();
    run(&first, "INSERT (:A {n: 1})");
    let error = Database::open(&path).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::DatabaseAlreadyOpen, "{error}");
    let mut tx = first.begin_write().unwrap();
    tx.execute("INSERT (:A {n: 2})", &Parameters::new())
        .unwrap();
    assert_eq!(
        Database::open(&path).unwrap_err().kind(),
        ErrorKind::DatabaseAlreadyOpen
    );
    tx.commit().unwrap();
    assert_eq!(single(&first, "MATCH (a:A) RETURN sum(a.n)"), int(3));
    drop(first);
    let again = Database::open(&path).unwrap();
    assert_eq!(single(&again, "MATCH (a:A) RETURN sum(a.n)"), int(3));
}

#[test]
fn opening_a_file_another_process_holds_open_fails_and_corrupts_nothing() {
    let test = "opening_a_file_another_process_holds_open_fails_and_corrupts_nothing";
    if let Ok(path) = std::env::var("RHIZODB_HELD_FILE") {
        let error = Database::open(&path).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::DatabaseAlreadyOpen, "{error}");
        return;
    }
    let (_directory, path) = graph_file();
    let db = Database::open(&path).unwrap();
    run(&db, "UNWIND range(1, 100) AS i INSERT (:A {i: i})");
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([test, "--exact", "--nocapture"])
        .env("RHIZODB_HELD_FILE", &path)
        .status()
        .unwrap();
    assert!(status.success());
    run(&db, "INSERT (:A {i: 0})");
    drop(db);
    let db = Database::open(&path).unwrap();
    assert_eq!(
        run(&db, "MATCH (a:A) RETURN count(*), sum(a.i)"),
        [vec![int(101), int(5050)]]
    );
}

#[test]
fn opening_what_is_not_a_database_file_fails_without_touching_it() {
    let directory = tempfile::tempdir().unwrap();
    assert_eq!(
        Database::open(directory.path()).unwrap_err().kind(),
        ErrorKind::Storage
    );
    assert_eq!(
        Database::open(directory.path().join("missing/graph.redb"))
            .unwrap_err()
            .kind(),
        ErrorKind::Storage
    );
    let junk = directory.path().join("junk");
    let bytes: Vec<u8> = (0..20_000_u32).map(|i| (i * 7919 % 251) as u8).collect();
    std::fs::write(&junk, &bytes).unwrap();
    let error = Database::open(&junk).unwrap_err();
    assert!(
        matches!(error.kind(), ErrorKind::Storage | ErrorKind::CorruptData),
        "{error}"
    );
    assert_eq!(std::fs::read(&junk).unwrap(), bytes);
    let empty = directory.path().join("empty");
    std::fs::write(&empty, b"").unwrap();
    let db = Database::open(&empty).unwrap();
    run(&db, "INSERT (:A)");
    assert_eq!(single(&db, "MATCH (n) RETURN count(*)"), int(1));
}

#[test]
fn a_forgotten_write_transaction_commits_nothing_and_its_thread_cannot_wait_for_it() {
    let (_directory, path) = graph_file();
    {
        let db = Database::open(&path).unwrap();
        run(&db, "INSERT (:Kept)");
        let mut tx = db.begin_write().unwrap();
        tx.execute("INSERT (:Forgotten)", &Parameters::new())
            .unwrap();
        std::mem::forget(tx);
        assert_eq!(
            db.begin_write().unwrap_err().kind(),
            ErrorKind::WriteTransactionOnThisThread
        );
        assert_eq!(
            db.execute("INSERT (:Other)", &Parameters::new())
                .unwrap_err()
                .kind(),
            ErrorKind::WriteTransactionOnThisThread
        );
        assert_eq!(
            run(&db, "MATCH (n) RETURN labels(n)"),
            [vec![list([string("Kept")])]]
        );
    }
    let error = Database::open(&path).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::DatabaseAlreadyOpen, "{error}");
}

#[test]
fn a_forgotten_read_transaction_lets_the_file_close_and_reopen() {
    let (_directory, path) = graph_file();
    {
        let db = Database::open(&path).unwrap();
        run(&db, "INSERT (:Kept)");
        std::mem::forget(db.begin_read().unwrap());
        run(&db, "INSERT (:Later)");
    }
    match Database::open(&path) {
        Ok(db) => assert_eq!(single(&db, "MATCH (n) RETURN count(*)"), int(2)),
        Err(error) => assert_eq!(error.kind(), ErrorKind::DatabaseAlreadyOpen, "{error}"),
    }
}

fn callback_database(probe: &'static str) -> (Arc<Database>, Arc<OnceLock<Arc<Database>>>) {
    let target: Arc<OnceLock<Arc<Database>>> = Arc::default();
    let db = db();
    let reached = Arc::clone(&target);
    db.register_procedure(
        Procedure::new("test.reenter", move |_| {
            let db = reached.get().ok_or("no database yet")?;
            let result = db.execute(probe, &Parameters::new())?;
            Ok(vec![vec![Value::Integer(result.rows().len() as i64)]].into())
        })
        .output("rows"),
    )
    .unwrap();
    let db = Arc::new(db);
    target.set(Arc::clone(&db)).ok().unwrap();
    (db, target)
}

#[test]
fn a_procedure_that_calls_back_into_its_database_reads_but_cannot_write_from_a_write() {
    let (db, _) = callback_database("INSERT (:FromProcedure)");
    assert_eq!(
        single(&db, "CALL test.reenter() YIELD rows RETURN rows"),
        int(0)
    );
    let error = db
        .execute(
            "INSERT (:Outer) WITH 1 AS x CALL test.reenter() YIELD rows RETURN rows",
            &Parameters::new(),
        )
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::ProcedureFailed, "{error}");
    assert_eq!(
        run(&db, "MATCH (n) RETURN labels(n)"),
        [vec![list([string("FromProcedure")])]]
    );
    let (db, _) = callback_database("MATCH (n) RETURN n");
    run(&db, "INSERT (:A), (:B)");
    assert_eq!(
        single(
            &db,
            "INSERT (:C) WITH 1 AS x CALL test.reenter() YIELD rows RETURN rows"
        ),
        int(2)
    );
}

#[test]
fn a_panicking_procedure_unwinds_out_and_leaves_the_database_usable() {
    let db = db();
    db.register_procedure(
        Procedure::new("test.boom", |_| panic!("the host's procedure panicked")).output("x"),
    )
    .unwrap();
    run(&db, "INSERT (:Before)");
    for query in [
        "CALL test.boom() YIELD x RETURN x",
        "INSERT (:During) WITH 1 AS y CALL test.boom() YIELD x RETURN x",
    ] {
        let unwound =
            std::panic::catch_unwind(AssertUnwindSafe(|| db.execute(query, &Parameters::new())));
        assert!(unwound.is_err(), "{query}");
    }
    let unwound = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let mut tx = db.begin_write().unwrap();
        tx.execute("INSERT (:InTransaction)", &Parameters::new())
            .unwrap();
        tx.execute("CALL test.boom() YIELD x RETURN x", &Parameters::new())
    }));
    assert!(unwound.is_err());
    run(&db, "INSERT (:After)");
    assert_eq!(
        run(&db, "MATCH (n) RETURN labels(n)[0] AS l ORDER BY l"),
        [vec![string("After")], vec![string("Before")]]
    );
    let other = thread::scope(|scope| {
        scope
            .spawn(|| db.execute("INSERT (:Other)", &Parameters::new()).map(drop))
            .join()
            .unwrap()
    });
    assert!(other.is_ok());
}

#[test]
fn updating_queries_in_a_read_transaction_fail_before_reading_and_keep_it_usable() {
    let db = db();
    run(&db, "INSERT (:A {n: 1})");
    let snapshot = db.begin_read().unwrap();
    for query in [
        "INSERT (:B)",
        "MATCH (a:A) SET a.n = 2",
        "MATCH (a:A) DETACH DELETE a",
        "MERGE (:C)",
        "MATCH (a:A) REMOVE a:A",
    ] {
        assert_eq!(
            snapshot
                .execute(query, &Parameters::new())
                .unwrap_err()
                .kind(),
            ErrorKind::ReadOnlyTransaction,
            "{query}"
        );
    }
    db.create_property_index("by_n", IndexTarget::Label("A"), "n")
        .unwrap();
    assert!(db.drop_index("by_n").unwrap());
    assert!(!db.drop_index("by_n").unwrap());
    assert_eq!(
        snapshot
            .execute("MATCH (a:A) RETURN a.n", &Parameters::new())
            .unwrap()
            .rows(),
        [vec![int(1)]]
    );
}

#[test]
fn invalid_index_definitions_and_searches_fail_with_their_error_kinds() {
    let db = db();
    run(&db, "INSERT (:V {v: [1.0, 2.0], s: 'some words'})");
    for (outcome, kind) in [
        (
            db.create_vector_index(
                "huge",
                IndexTarget::Label("V"),
                "v",
                usize::MAX,
                VectorMetric::Cosine,
            ),
            ErrorKind::InvalidIndexDefinition,
        ),
        (
            db.create_vector_index(
                "zero",
                IndexTarget::Label("V"),
                "v",
                0,
                VectorMetric::Cosine,
            ),
            ErrorKind::InvalidIndexDefinition,
        ),
        (
            db.create_keyword_index("none", IndexTarget::Label("V"), &[]),
            ErrorKind::InvalidIndexDefinition,
        ),
        (
            db.create_property_index("", IndexTarget::Label("V"), "v"),
            ErrorKind::InvalidIndexDefinition,
        ),
    ] {
        assert_eq!(outcome.unwrap_err().kind(), kind);
    }
    db.create_vector_index("v", IndexTarget::Label("V"), "v", 2, VectorMetric::Cosine)
        .unwrap();
    db.create_keyword_index(&"n".repeat(1 << 16), IndexTarget::Label("V"), &["s"])
        .unwrap();
    assert_eq!(
        db.create_keyword_index("v", IndexTarget::Label("V"), &["s"])
            .unwrap_err()
            .kind(),
        ErrorKind::IndexAlreadyExists
    );
    let search = |vector: Value, limit: i64| {
        db.execute(
            "CALL db.index.vector.queryNodes('v', $v, $k) YIELD node RETURN count(node)",
            &Parameters::from([("v".to_owned(), vector), ("k".to_owned(), int(limit))]),
        )
        .map(|result| result.rows().to_vec())
        .map_err(|error| error.kind())
    };
    let floats = |values: &[f64]| list(values.iter().copied().map(Value::Float));
    assert_eq!(
        search(floats(&[1.0]), 10),
        Err(ErrorKind::InvalidArgumentValue)
    );
    assert_eq!(
        search(floats(&[f64::NAN, 1.0]), 10),
        Err(ErrorKind::InvalidArgumentValue)
    );
    assert_eq!(
        search(floats(&[f64::INFINITY, 1.0]), 10),
        Err(ErrorKind::InvalidArgumentValue)
    );
    assert_eq!(
        search(floats(&[1.0, 1.0]), -1),
        Err(ErrorKind::InvalidArgumentValue)
    );
    assert_eq!(
        search(floats(&[1.0, 1.0]), i64::MAX),
        Ok(vec![vec![int(1)]])
    );
    assert_eq!(
        error_kind(
            &db,
            "CALL db.index.keyword.queryNodes('missing', 'x', 10) YIELD node RETURN node"
        ),
        ErrorKind::IndexNotFound
    );
    assert_eq!(
        error_kind(
            &db,
            "CALL db.index.keyword.queryNodes('v', 'x', 10) YIELD node RETURN node"
        ),
        ErrorKind::IndexNotFound
    );
}

#[test]
fn missing_extra_and_odd_parameters_are_handled_without_a_panic() {
    let db = db();
    assert_eq!(
        error_kind(&db, "RETURN $absent"),
        ErrorKind::MissingParameter
    );
    let parameters = Parameters::from([
        ("used".to_owned(), int(1)),
        ("unused".to_owned(), string("x")),
        (String::new(), int(2)),
        ("\u{0}".to_owned(), int(3)),
    ]);
    assert_eq!(
        db.execute("RETURN $used", &parameters).unwrap().rows(),
        [vec![int(1)]]
    );
    db.create_property_index("by_v", IndexTarget::Label("N"), "v")
        .unwrap();
    for value in [
        Value::Float(f64::NAN),
        Value::Float(f64::INFINITY),
        Value::Float(-0.0),
        Value::Integer(i64::MIN),
        Value::String("\u{0}\u{fffd}\u{10ffff}".to_owned()),
        Value::Null,
        list([]),
    ] {
        let parameters = Parameters::from([("v".to_owned(), value.clone())]);
        db.execute("INSERT (:N {v: $v})", &parameters).unwrap();
        db.execute("MATCH (n:N {v: $v}) RETURN count(*)", &parameters)
            .unwrap();
        db.execute("MATCH (n:N) WHERE n.v >= $v RETURN count(*)", &parameters)
            .unwrap();
    }
    assert_eq!(single(&db, "MATCH (n:N) RETURN count(n.v)"), int(6));
    assert_eq!(single(&db, "MATCH (n:N {v: 0}) RETURN count(*)"), int(1));
}

fn names_in(directory: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn creating_a_database_leaves_only_its_file_and_replaces_a_staging_file_an_interrupted_open_left() {
    let (directory, path) = graph_file();
    std::fs::write(
        directory.path().join(".graph.redb.creating"),
        vec![9_u8; 5_000],
    )
    .unwrap();
    {
        let db = Database::open(&path).unwrap();
        run(&db, "INSERT (:A)");
    }
    assert_eq!(names_in(directory.path()), ["graph.redb"]);
    let db = Database::open(&path).unwrap();
    assert_eq!(single(&db, "MATCH (n:A) RETURN count(*)"), int(1));
}

#[test]
fn a_failed_creation_leaves_nothing_behind() {
    let directory = tempfile::tempdir().unwrap();
    let missing = directory.path().join("missing");
    assert_eq!(
        Database::open(missing.join("graph.redb"))
            .unwrap_err()
            .kind(),
        ErrorKind::Storage
    );
    assert_eq!(names_in(directory.path()), Vec::<String>::new());
    assert_eq!(
        Database::open(directory.path().join(".."))
            .unwrap_err()
            .kind(),
        ErrorKind::Storage
    );
}

#[test]
fn threads_racing_to_create_one_file_end_with_one_database_that_keeps_every_write() {
    for _ in 0..cases(5) {
        let (directory, path) = graph_file();
        let written = thread::scope(|scope| {
            let openers: Vec<_> = (0..8)
                .map(|writer| {
                    let path = &path;
                    scope.spawn(move || match Database::open(path) {
                        Ok(db) => {
                            db.execute(
                                "INSERT (:W {w: $w})",
                                &Parameters::from([("w".to_owned(), int(writer))]),
                            )
                            .unwrap();
                            1
                        }
                        Err(error) => {
                            assert_eq!(error.kind(), ErrorKind::DatabaseAlreadyOpen, "{error}");
                            0
                        }
                    })
                })
                .collect();
            openers
                .into_iter()
                .map(|opener| opener.join().unwrap())
                .sum::<i64>()
        });
        assert!(written >= 1);
        assert_eq!(names_in(directory.path()), ["graph.redb"]);
        let db = Database::open(&path).unwrap();
        assert_eq!(single(&db, "MATCH (n:W) RETURN count(*)"), int(written));
    }
}
