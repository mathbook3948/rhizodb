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
