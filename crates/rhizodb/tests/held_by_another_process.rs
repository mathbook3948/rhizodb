//! Opens a database that another process holds open. It runs in a test binary of its own,
//! because the child process it spawns briefly holds a copy of every file the parent has open,
//! and so the locks of databases that tests running beside it have just closed.

mod support;

use rhizodb::Database;
use rhizodb::ErrorKind;
use support::*;

fn graph_file() -> (tempfile::TempDir, std::path::PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph.redb");
    (directory, path)
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
