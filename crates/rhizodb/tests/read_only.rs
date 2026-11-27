//! Opens database files read-only, in this process and in child processes, and checks that
//! read-only opens share a file, keep out an open that writes, refuse every write, and leave
//! every file as it was.

mod support;

use std::collections::BTreeMap;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::ChildStdout;
use std::process::Command;
use std::process::Stdio;
use std::time::Duration;

use rhizodb::Database;
use rhizodb::ErrorKind;
use rhizodb::IndexTarget;
use rhizodb::OpenOptions;
use rhizodb::Parameters;
use rhizodb::Position;
use rhizodb::Procedure;
use rhizodb::VectorMetric;
use support::*;

const DATABASE_ENV: &str = "RHIZODB_READ_ONLY_DATABASE";
const MODE_ENV: &str = "RHIZODB_READ_ONLY_MODE";

fn read_only() -> OpenOptions {
    OpenOptions::new().read_only(true)
}

/// Every file in `directory` with its bytes.
fn files(directory: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            let bytes = std::fs::read(&path).unwrap();
            (path, bytes)
        })
        .collect()
}

fn graph_file() -> (tempfile::TempDir, PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph.rhizodb");
    let db = Database::open(&path).unwrap();
    db.create_property_index("item_i", IndexTarget::Label("Item"), "i")
        .unwrap();
    run(
        &db,
        "UNWIND range(1, 100) AS i INSERT (:Item {i: i, text: 'item ' + toString(i)})",
    );
    (directory, path)
}

fn count(db: &Database) -> i64 {
    ints(&run(db, "MATCH (n:Item) RETURN count(n)"))[0]
}

#[test]
fn a_read_only_database_refuses_every_write_serves_every_read_and_changes_no_file() {
    let (directory, path) = graph_file();
    let backup = tempfile::tempdir().unwrap();
    let before = files(directory.path());
    let writer = Database::in_memory().unwrap();
    run(&writer, "INSERT (:Item {i: 0})");
    let changes = writer.changes_since(&Position::start(), 1 << 20).unwrap();
    let mut dump = Vec::new();
    writer.export(&mut dump).unwrap();
    {
        let db = read_only().open(&path).unwrap();
        let none = Parameters::new();
        let refusals = [
            ("execute", db.execute("INSERT (:Item)", &none).map(drop)),
            ("begin_write", db.begin_write().map(drop)),
            (
                "create_property_index",
                db.create_property_index("other", IndexTarget::Label("Item"), "text"),
            ),
            (
                "create_keyword_index",
                db.create_keyword_index("words", IndexTarget::Label("Item"), &["text"]),
            ),
            (
                "create_vector_index",
                db.create_vector_index(
                    "near",
                    IndexTarget::Label("Item"),
                    "v",
                    2,
                    VectorMetric::Cosine,
                ),
            ),
            ("drop_index", db.drop_index("item_i").map(drop)),
            ("compact", db.compact()),
            ("import", db.import(dump.as_slice())),
            ("acknowledge", db.acknowledge(b"peer", &db.position())),
            ("forget_peer", db.forget_peer(b"peer").map(drop)),
            ("apply", db.apply(&changes).map(drop)),
            ("promote", db.promote(1, b"writer").map(drop)),
            ("demote", db.demote(Duration::ZERO).map(drop)),
            (
                "install_snapshot",
                read_only()
                    .install_snapshot(directory.path().join("installed.rhizodb"), &[][..])
                    .map(drop),
            ),
        ];
        for (call, result) in refusals {
            let error = result.expect_err(call);
            assert_eq!(error.kind(), ErrorKind::ReadOnly, "{call}: {error}");
        }
        assert_eq!(
            db.execute("INSERT (:Item)", &none).unwrap_err().kind(),
            ErrorKind::ReadOnly,
            "a refused demotion leaves writes refused as read-only"
        );
        let error = read_only().open(&path).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::DatabaseAlreadyOpen, "{error}");
        assert_eq!(count(&db), 100);
        assert_eq!(
            ints(&run(&db, "MATCH (n:Item {i: 42}) RETURN n.i")),
            [42],
            "the property index serves reads"
        );
        let snapshot = db.begin_read().unwrap();
        assert_eq!(
            ints(
                snapshot
                    .execute("MATCH (n) RETURN count(n)", &none)
                    .unwrap()
                    .rows()
            ),
            [100]
        );
        drop(snapshot);
        let mut exported = Vec::new();
        db.export(&mut exported).unwrap();
        let copy = Database::in_memory().unwrap();
        copy.import(exported.as_slice()).unwrap();
        assert_eq!(count(&copy), 100);
        let stream = db.snapshot().unwrap();
        let mut bytes = vec![0; usize::try_from(stream.len()).unwrap()];
        let mut offset = 0;
        while offset < bytes.len() {
            offset += stream.read_at(offset as u64, &mut bytes[offset..]).unwrap();
        }
        drop(stream);
        let installed = backup.path().join("installed.rhizodb");
        assert_eq!(
            count(&Database::install_snapshot(&installed, bytes.as_slice()).unwrap()),
            100
        );
        let backed_up = backup.path().join("backup.rhizodb");
        db.backup_to(&backed_up).unwrap();
        assert_eq!(count(&Database::open(&backed_up).unwrap()), 100);
        assert!(db.stats().unwrap().file_bytes > 0);
        db.check().unwrap();
        db.register_procedure(Procedure::new("test.nothing", |_| Ok(Vec::new().into())))
            .unwrap();
        assert_eq!(db.changes_since(&db.position(), 1 << 20).unwrap().len(), 0);
    }
    let error = read_only().replica(true).open(&path).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NotReplica, "{error}");
    assert_eq!(files(directory.path()), before);
}

#[test]
fn a_read_only_open_creates_no_file_where_there_is_no_database() {
    let directory = tempfile::tempdir().unwrap();
    let missing = directory.path().join("missing.rhizodb");
    let empty = directory.path().join("empty.rhizodb");
    std::fs::write(&empty, b"").unwrap();
    let before = files(directory.path());
    for path in [&missing, &empty] {
        let error = read_only().open(path).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::DatabaseNotFound, "{error}");
        let error = read_only().create(true).open(path).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::DatabaseNotFound, "{error}");
    }
    assert_eq!(files(directory.path()), before);
    assert_eq!(
        read_only().in_memory().unwrap_err().kind(),
        ErrorKind::DatabaseNotFound
    );
}

#[test]
fn a_file_in_an_older_graph_format_fails_to_open_read_only_and_stays_as_it_was() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph.rhizodb");
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/format-13.db"),
        &path,
    )
    .unwrap();
    let before = files(directory.path());
    let error = read_only().open(&path).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::ReadOnly, "{error}");
    assert_eq!(files(directory.path()), before);
    let upgraded = Database::open(&path).unwrap();
    drop(upgraded);
    read_only().open(&path).unwrap();
}

#[test]
#[ignore = "child process of the read-only process tests, selected by RHIZODB_READ_ONLY_DATABASE"]
fn read_only_child() {
    let Ok(path) = std::env::var(DATABASE_ENV) else {
        return;
    };
    let db = match std::env::var(MODE_ENV).as_deref() {
        Ok("write") => Database::open(&path).unwrap(),
        _ => read_only().open(&path).unwrap(),
    };
    let mut stdout = std::io::stdout();
    writeln!(stdout, "ready {}", count(&db)).unwrap();
    stdout.flush().unwrap();
    let mut rest = Vec::new();
    std::io::stdin().read_to_end(&mut rest).unwrap();
    writeln!(stdout, "still {}", count(&db)).unwrap();
    stdout.flush().unwrap();
}

/// A child process that holds the database open until it is told to close it.
struct Holder {
    child: Child,
    stdout: BufReader<ChildStdout>,
}

impl Holder {
    fn spawn(path: &Path, mode: &str) -> Self {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["read_only_child", "--exact", "--ignored", "--nocapture"])
            .env(DATABASE_ENV, path)
            .env(MODE_ENV, mode)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut holder = Self { child, stdout };
        assert_eq!(holder.line_starting("ready "), "100");
        holder
    }

    fn line_starting(&mut self, prefix: &str) -> String {
        loop {
            let mut line = String::new();
            assert!(
                self.stdout.read_line(&mut line).unwrap() > 0,
                "the child exited before printing {prefix:?}"
            );
            if let Some(rest) = line.trim_end().strip_prefix(prefix) {
                return rest.to_owned();
            }
        }
    }

    /// Lets the child read once more and close the database, and waits for it to exit.
    fn close(mut self) {
        drop(self.child.stdin.take());
        assert_eq!(self.line_starting("still "), "100");
        assert!(self.child.wait().unwrap().success());
    }
}

impl Drop for Holder {
    fn drop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
    }
}

#[test]
fn read_only_processes_share_the_file_and_keep_out_a_writer_that_keeps_them_out() {
    let (directory, path) = graph_file();
    let before = files(directory.path());
    let first = Holder::spawn(&path, "read");
    let second = Holder::spawn(&path, "read");
    assert_eq!(count(&read_only().open(&path).unwrap()), 100);
    let error = Database::open(&path).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::DatabaseAlreadyOpen, "{error}");
    first.close();
    let error = Database::open(&path).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::DatabaseAlreadyOpen, "{error}");
    second.close();
    assert_eq!(files(directory.path()), before);
    let writer = Holder::spawn(&path, "write");
    let error = read_only().open(&path).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::DatabaseAlreadyOpen, "{error}");
    writer.close();
    assert_eq!(count(&read_only().open(&path).unwrap()), 100);
}

#[test]
fn a_file_of_an_older_engine_minor_is_read_as_it_is_and_left_unchanged() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph.rhizodb");
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/engine-1-1.rhizodb"),
        &path,
    )
    .unwrap();
    let before = files(directory.path());
    // The engine reads the old minor without a rewrite, so the open reaches the graph, which
    // refuses the fixture's older graph format.
    let error = read_only().open(&path).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::ReadOnly, "{error}");
    assert!(error.to_string().contains("older format"), "{error}");
    assert_eq!(files(directory.path()), before);
}
