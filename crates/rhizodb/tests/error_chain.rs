//! A reporter that walks an error's sources prints each cause once.

use std::error::Error as StdError;
use std::io;
use std::io::Write;

use rhizodb::Database;
use rhizodb::Error;
use rhizodb::ErrorKind;
use rhizodb::Parameters;
use rhizodb::Procedure;

fn chain(error: &Error) -> Vec<String> {
    let mut messages = vec![error.to_string()];
    let mut source = error.source();
    while let Some(cause) = source {
        messages.push(cause.to_string());
        source = cause.source();
    }
    for pair in messages.windows(2) {
        assert!(!pair[0].contains(&pair[1]), "a cause repeats: {messages:?}");
    }
    messages
}

#[test]
fn a_storage_failure_states_its_cause_once() {
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("backup.rhizodb");
    std::fs::write(&target, b"taken").unwrap();
    let error = Database::in_memory()
        .unwrap()
        .backup_to(&target)
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::BackupTargetExists);
    let messages = chain(&error);
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert!(messages[0].contains("already exists"), "{messages:?}");
}

#[test]
fn a_graph_failure_states_its_cause_once() {
    let db = Database::in_memory().unwrap();
    let _write = db.begin_write().unwrap();
    let error = db.begin_write().unwrap_err();
    assert_eq!(error.kind(), ErrorKind::WriteTransactionOnThisThread);
    assert_eq!(chain(&error).len(), 1);
}

#[test]
fn a_procedure_failure_names_the_procedure_and_leaves_the_reason_to_its_source() {
    let db = Database::in_memory().unwrap();
    db.register_procedure(Procedure::new("test.fail", |_| Err("not today".into())))
        .unwrap();
    let error = db
        .execute("CALL test.fail()", &Parameters::new())
        .unwrap_err();
    assert_eq!(chain(&error), ["procedure `test.fail` failed", "not today"]);
}

struct Full;

impl Write for Full {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::new(
            io::ErrorKind::StorageFull,
            "the disk is full",
        ))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn a_failed_export_leaves_the_io_error_to_its_source() {
    let db = Database::in_memory().unwrap();
    let error = db.export(Full).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io);
    let messages = chain(&error);
    assert_eq!(
        messages.last().map(String::as_str),
        Some("the disk is full")
    );
    let io = error.source().unwrap().downcast_ref::<io::Error>().unwrap();
    assert_eq!(io.kind(), io::ErrorKind::StorageFull);
}

struct Dropped;

impl io::Read for Dropped {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        Err(io::Error::new(
            io::ErrorKind::ConnectionReset,
            "peer went away",
        ))
    }
}

fn io_source(error: &Error) -> &io::Error {
    error
        .source()
        .and_then(|source| source.downcast_ref::<io::Error>())
        .unwrap_or_else(|| panic!("no I/O source: {error}"))
}

#[test]
fn a_failed_snapshot_read_leaves_the_io_error_to_its_source() {
    let directory = tempfile::tempdir().unwrap();
    let error =
        Database::install_snapshot(directory.path().join("replica.rhizodb"), Dropped).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io);
    let messages = chain(&error);
    assert_eq!(messages.len(), 2, "{messages:?}");
    assert_eq!(messages[1], "peer went away");
    assert_eq!(io_source(&error).kind(), io::ErrorKind::ConnectionReset);
}

#[test]
fn an_open_in_a_missing_directory_leaves_the_io_error_to_its_source() {
    let directory = tempfile::tempdir().unwrap();
    let error = Database::open(directory.path().join("missing").join("graph.rhizodb")).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Storage);
    let messages = chain(&error);
    assert_eq!(messages.len(), 2, "{messages:?}");
    assert!(
        messages[0].starts_with("cannot open the database at"),
        "{messages:?}"
    );
    assert_eq!(io_source(&error).kind(), io::ErrorKind::NotFound);
}

#[test]
fn a_backup_into_a_missing_directory_leaves_the_io_error_to_its_source() {
    let directory = tempfile::tempdir().unwrap();
    let error = Database::in_memory()
        .unwrap()
        .backup_to(directory.path().join("missing").join("backup.rhizodb"))
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Storage);
    let messages = chain(&error);
    assert_eq!(messages.len(), 2, "{messages:?}");
    assert!(
        messages[0].starts_with("cannot write the backup to"),
        "{messages:?}"
    );
    assert_eq!(io_source(&error).kind(), io::ErrorKind::NotFound);
}

#[test]
fn a_path_that_cannot_be_checked_states_its_cause_once() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("file");
    std::fs::write(&file, b"not a directory").unwrap();
    let error = rhizodb::OpenOptions::new()
        .create(false)
        .open(file.join("graph.rhizodb"))
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Storage);
    let messages = chain(&error);
    assert_eq!(messages.len(), 2, "{messages:?}");
    assert_eq!(io_source(&error).kind(), io::ErrorKind::NotADirectory);
}

#[test]
fn a_replication_refusal_states_its_cause_once() {
    let writer = Database::in_memory().unwrap();
    writer.promote(1, b"node-a").unwrap();
    let changes = writer
        .changes_since(&rhizodb::Position::start(), 1 << 20)
        .unwrap();
    let error = Database::in_memory().unwrap().apply(&changes).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NotReplica);
    assert_eq!(chain(&error).len(), 1);
}
