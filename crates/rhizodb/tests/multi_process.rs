//! Opens one database file from several child processes at once, one writing with
//! `OpenOptions::multi_process` and the rest read-only, kills them at random points, and checks
//! that every reader reads a whole prefix of the reported commits, that a killed writer loses no
//! reported commit, and that the file reopened alone passes `check`.

mod support;

use std::io::BufRead;
use std::io::BufReader;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::process::Child;
use std::process::ChildStdout;
use std::process::Command;
use std::process::Stdio;
use std::time::Duration;

use rhizodb::Database;
use rhizodb::ErrorKind;
use rhizodb::OpenOptions;
use support::*;

const DATABASE_ENV: &str = "RHIZODB_MULTI_PROCESS_DATABASE";
const MODE_ENV: &str = "RHIZODB_MULTI_PROCESS_MODE";

fn writer_options() -> OpenOptions {
    OpenOptions::new().multi_process(true)
}

fn reader_options() -> OpenOptions {
    OpenOptions::new().read_only(true)
}

/// The rounds a database holds, which must be every round from zero to the last.
fn rounds(db: &Database) -> i64 {
    let found = ints(&run(db, "MATCH (r:Round) RETURN count(r)"))[0];
    let last = ints(&run(db, "MATCH (r:Round) RETURN coalesce(max(r.n), -1)"))[0];
    assert_eq!(found, last + 1, "the rounds are not a whole prefix");
    found
}

#[test]
#[ignore = "child process of the multi-process tests, selected by RHIZODB_MULTI_PROCESS_DATABASE"]
fn multi_process_child() {
    let Ok(path) = std::env::var(DATABASE_ENV) else {
        return;
    };
    let mut stdout = std::io::stdout();
    match std::env::var(MODE_ENV).as_deref() {
        Ok("write") => {
            let db = writer_options().open(&path).unwrap();
            let start = rounds(&db);
            writeln!(stdout, "ready {start}").unwrap();
            stdout.flush().unwrap();
            for round in start..i64::MAX {
                run(
                    &db,
                    &format!(
                        "INSERT (:Round {{n: {round}, pad: '{}'}})",
                        "x".repeat(2000)
                    ),
                );
                if round % 30 == 29 {
                    db.compact().unwrap();
                }
                writeln!(stdout, "committed {round}").unwrap();
                stdout.flush().unwrap();
            }
        }
        _ => {
            let db = reader_options().open(&path).unwrap();
            let seen = rounds(&db);
            writeln!(stdout, "ready {seen}").unwrap();
            stdout.flush().unwrap();
            let mut rest = Vec::new();
            std::io::stdin().read_to_end(&mut rest).unwrap();
            writeln!(stdout, "still {}", rounds(&db)).unwrap();
            stdout.flush().unwrap();
        }
    }
}

struct Peer {
    child: Child,
    stdout: BufReader<ChildStdout>,
    committed: Vec<i64>,
}

impl Peer {
    fn spawn(path: &Path, mode: &str) -> (Self, i64) {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["multi_process_child", "--exact", "--ignored", "--nocapture"])
            .env(DATABASE_ENV, path)
            .env(MODE_ENV, mode)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut peer = Self {
            child,
            stdout,
            committed: Vec::new(),
        };
        let ready = peer.line_starting("ready ").parse().unwrap();
        (peer, ready)
    }

    fn line_starting(&mut self, prefix: &str) -> String {
        loop {
            let mut line = String::new();
            assert!(
                self.stdout.read_line(&mut line).unwrap() > 0,
                "the child exited before printing {prefix:?}"
            );
            let line = line.trim_end();
            if let Some(round) = line.strip_prefix("committed ") {
                self.committed.push(round.parse().unwrap());
            }
            if let Some(rest) = line.strip_prefix(prefix) {
                return rest.to_owned();
            }
        }
    }

    fn wait_for_commits(&mut self, at_least: usize) {
        while self.committed.len() < at_least {
            self.line_starting("committed ");
        }
    }

    /// Lets a reader read once more and close, and returns what it read the second time.
    fn close_reader(mut self) -> i64 {
        drop(self.child.stdin.take());
        let still = self.line_starting("still ").parse().unwrap();
        assert!(self.child.wait().unwrap().success());
        still
    }

    /// Kills the child and returns every round it reported as committed.
    fn kill(mut self) -> Vec<i64> {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
        let mut rest = String::new();
        self.stdout.read_to_string(&mut rest).ok();
        for line in rest.lines() {
            if let Some(round) = line.strip_prefix("committed ") {
                self.committed.push(round.parse().unwrap());
            }
        }
        std::mem::take(&mut self.committed)
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
    }
}

fn next(seed: &mut u64) -> u64 {
    *seed = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *seed;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[test]
fn readers_beside_a_writer_killed_at_random_points_read_whole_prefixes_and_lose_no_commit() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph.rhizodb");
    let mut seed = seed();
    let mut reported = 0;
    for life in 0..4 {
        let (mut writer, start) = Peer::spawn(&path, "write");
        assert!(
            start >= reported,
            "life {life}: the writer lost reported commits"
        );
        let mut readers = Vec::new();
        for _ in 0..3 {
            writer.wait_for_commits(1 + (next(&mut seed) % 20) as usize);
            let before = writer.committed.last().copied().unwrap_or(start - 1);
            let (reader, seen) = Peer::spawn(&path, "read");
            assert!(
                seen > before,
                "life {life}: a reader missed a reported commit"
            );
            readers.push((reader, seen));
            let in_parent = rounds(&reader_options().open(&path).unwrap());
            assert!(in_parent >= seen, "life {life}: a later reader read less");
        }
        let error = writer_options().open(&path).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::DatabaseAlreadyOpen, "{error}");
        std::thread::sleep(Duration::from_millis(next(&mut seed) % 30));
        let committed = writer.kill();
        reported = committed.last().map_or(reported, |last| last + 1);
        let victim = (next(&mut seed) % readers.len() as u64) as usize;
        readers.swap_remove(victim).0.kill();
        for (reader, seen) in readers {
            assert_eq!(
                reader.close_reader(),
                seen,
                "life {life}: a reader's snapshot moved"
            );
        }
    }
    let alone = Database::open(&path).unwrap();
    assert!(rounds(&alone) >= reported);
    alone.check().unwrap();
}

#[test]
fn a_writer_keeps_writing_and_reusing_space_while_readers_come_and_die() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph.rhizodb");
    let writer = writer_options().open(&path).unwrap();
    let mut seed = seed();
    let mut readers = Vec::new();
    for round in 0..300 {
        run(
            &writer,
            &format!(
                "INSERT (:Round {{n: {round}, pad: '{}'}})",
                "x".repeat(2000)
            ),
        );
        if round % 20 == 19 {
            writer.compact().unwrap();
        }
        if round % 25 == 0 {
            let (reader, seen) = Peer::spawn(&path, "read");
            assert_eq!(seen, round + 1);
            readers.push((reader, seen));
        }
        if round % 40 == 39 && !readers.is_empty() {
            let victim = (next(&mut seed) % readers.len() as u64) as usize;
            readers.swap_remove(victim).0.kill();
        }
    }
    for (reader, seen) in readers {
        assert_eq!(reader.close_reader(), seen);
    }
    assert_eq!(rounds(&writer), 300);
    writer.check().unwrap();
    drop(writer);
    let alone = Database::open(&path).unwrap();
    assert_eq!(rounds(&alone), 300);
    alone.check().unwrap();
}

#[test]
fn opens_by_a_new_name_are_refused_while_a_writer_s_session_holds_the_file_by_its_old_one() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph.rhizodb");
    let renamed = directory.path().join("renamed.rhizodb");
    drop(writer_options().open(&path).unwrap());
    let (mut writer, _) = Peer::spawn(&path, "write");
    writer.wait_for_commits(3);
    std::fs::rename(&path, &renamed).unwrap();
    for (what, options) in [
        ("a writer", writer_options()),
        ("a reader", reader_options()),
    ] {
        let error = options.open(&renamed).unwrap_err();
        assert_eq!(
            error.kind(),
            ErrorKind::DatabaseAlreadyOpen,
            "{what}: {error}"
        );
    }
    let committed = writer.kill();
    std::fs::rename(&renamed, &path).unwrap();
    let db = writer_options().open(&path).unwrap();
    assert!(rounds(&db) >= committed.len() as i64);
}
