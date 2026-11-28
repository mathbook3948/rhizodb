//! Opens one database file from several writing child processes and read-only ones at once,
//! kills writers at random points and, in a build made with `--cfg crash_points`, at every
//! named step of a commit, a hand-over of the log's tail, and a checkpoint, and checks that no
//! commit a writer reported is lost, that every process reads each writer's commits as a whole
//! prefix, and that the file reopened alone passes `check`. Every wait on a child is bounded, so
//! a hang fails and names what it waited for.

mod support;

use std::collections::BTreeMap;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::sync::mpsc;
use std::time::Duration;
use std::time::Instant;

use rhizodb::Database;
use rhizodb::OpenOptions;
use rhizodb::Value;
use support::*;

const DATABASE_ENV: &str = "RHIZODB_MULTI_WRITER_DATABASE";
const MODE_ENV: &str = "RHIZODB_MULTI_WRITER_MODE";
const TAG_ENV: &str = "RHIZODB_MULTI_WRITER_TAG";
const COMPACT_ENV: &str = "RHIZODB_MULTI_WRITER_COMPACT";
const PAD_ENV: &str = "RHIZODB_MULTI_WRITER_PAD";

const PATIENCE: Duration = Duration::from_secs(60);

fn writer_options() -> OpenOptions {
    OpenOptions::new().multi_process(true)
}

fn reader_options() -> OpenOptions {
    OpenOptions::new().read_only(true)
}

/// How many commits each writer has in `db`, checking that each writer's are a whole prefix.
fn commits(db: &Database) -> BTreeMap<i64, i64> {
    let rows = run(
        db,
        "MATCH (c:Commit) RETURN c.w, count(c), max(c.n) ORDER BY c.w",
    );
    rows.iter()
        .map(|row| {
            let [
                Value::Integer(writer),
                Value::Integer(found),
                Value::Integer(last),
            ] = row[..]
            else {
                panic!("expected three integers, got {row:?}");
            };
            assert_eq!(
                found,
                last + 1,
                "the commits of writer {writer} are not a whole prefix"
            );
            (writer, found)
        })
        .collect()
}

#[test]
#[ignore = "child process of the multi-writer tests, selected by RHIZODB_MULTI_WRITER_DATABASE"]
fn multi_writer_child() {
    let Ok(path) = std::env::var(DATABASE_ENV) else {
        return;
    };
    let mut stdout = std::io::stdout();
    match std::env::var(MODE_ENV).as_deref() {
        Ok("write") => {
            let tag: i64 = std::env::var(TAG_ENV).unwrap().parse().unwrap();
            let compact: i64 = std::env::var(COMPACT_ENV).unwrap().parse().unwrap();
            let pad: usize = std::env::var(PAD_ENV).unwrap().parse().unwrap();
            let db = writer_options().open(&path).unwrap();
            let start = commits(&db).get(&tag).copied().unwrap_or(0);
            writeln!(stdout, "ready {start}").unwrap();
            stdout.flush().unwrap();
            for n in start..i64::MAX {
                run(
                    &db,
                    &format!(
                        "INSERT (:Commit {{w: {tag}, n: {n}, pad: '{}'}})",
                        "x".repeat(pad)
                    ),
                );
                writeln!(stdout, "committed {n}").unwrap();
                stdout.flush().unwrap();
                if n % compact == compact - 1 {
                    db.compact().unwrap();
                }
            }
        }
        _ => {
            let db = reader_options().open(&path).unwrap();
            let seen: i64 = commits(&db).values().sum();
            writeln!(stdout, "ready {seen}").unwrap();
            stdout.flush().unwrap();
            let mut rest = Vec::new();
            std::io::stdin().read_to_end(&mut rest).unwrap();
            let still: i64 = commits(&db).values().sum();
            writeln!(stdout, "still {still}").unwrap();
            stdout.flush().unwrap();
        }
    }
}

struct Peer {
    child: Child,
    lines: mpsc::Receiver<String>,
    what: String,
    committed: Vec<i64>,
}

impl Peer {
    fn start(path: &Path, mode: &str, envs: &[(&str, String)]) -> Self {
        let what = format!("{mode} child {envs:?}");
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["multi_writer_child", "--exact", "--ignored", "--nocapture"])
            .env(DATABASE_ENV, path)
            .env(MODE_ENV, mode)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
        for (name, value) in envs {
            command.env(name, value);
        }
        let mut child = command.spawn().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let (sender, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in stdout.lines() {
                let Ok(line) = line else { break };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            lines,
            what,
            committed: Vec::new(),
        }
    }

    fn spawn(path: &Path, mode: &str, envs: &[(&str, String)]) -> (Self, i64) {
        let mut peer = Self::start(path, mode, envs);
        let ready = peer
            .line_starting("ready ")
            .unwrap_or_else(|| panic!("the {} exited before it was ready", peer.what))
            .parse()
            .unwrap();
        (peer, ready)
    }

    fn take(&mut self, line: &str) {
        if let Some(n) = line.strip_prefix("committed ") {
            self.committed.push(n.parse().unwrap());
        }
    }

    /// The rest of the first line starting with `prefix`, or `None` once the child exits
    /// without printing one.
    fn line_starting(&mut self, prefix: &str) -> Option<String> {
        loop {
            match self.lines.recv_timeout(PATIENCE) {
                Ok(line) => {
                    self.take(&line);
                    if let Some(rest) = line.strip_prefix(prefix) {
                        return Some(rest.to_owned());
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => return None,
                Err(mpsc::RecvTimeoutError::Timeout) => panic!(
                    "the {} printed nothing for {PATIENCE:?} while it was awaited for {prefix:?}, \
                     as a lock that a dead process kept would leave it",
                    self.what
                ),
            }
        }
    }

    fn wait_for_commits(&mut self, more: usize) {
        let until = self.committed.len() + more;
        while self.committed.len() < until {
            if self.line_starting("committed ").is_none() {
                panic!(
                    "the {} exited after {} commits",
                    self.what,
                    self.committed.len()
                );
            }
        }
    }

    #[cfg_attr(
        not(crash_points),
        expect(
            dead_code,
            reason = "only a crash-points build kills a writer at a named step"
        )
    )]
    /// Waits, within the bound, for a child that aborts itself, and returns every commit it
    /// reported, or panics naming `point` when it is still running.
    fn wait_for_abort(mut self, point: &str) -> Vec<i64> {
        let deadline = Instant::now() + PATIENCE;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(!status.success(), "the {} exited cleanly", self.what);
                while let Ok(line) = self.lines.recv_timeout(Duration::from_secs(5)) {
                    self.take(&line);
                }
                return std::mem::take(&mut self.committed);
            }
            while let Ok(line) = self.lines.try_recv() {
                self.take(&line);
            }
            assert!(
                Instant::now() < deadline,
                "the {} never reached crash point {point} within {PATIENCE:?}",
                self.what
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn close_reader(mut self) -> i64 {
        drop(self.child.stdin.take());
        let still = self
            .line_starting("still ")
            .unwrap_or_else(|| panic!("the {} exited before it read again", self.what))
            .parse()
            .unwrap();
        assert!(self.child.wait().unwrap().success());
        still
    }

    fn kill(mut self) -> Vec<i64> {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
        while let Ok(line) = self.lines.recv_timeout(Duration::from_secs(5)) {
            self.take(&line);
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

fn spawn_writer(path: &Path, tag: i64, extra: &[(&str, String)]) -> (Peer, i64) {
    let mut envs = vec![
        (TAG_ENV, tag.to_string()),
        (COMPACT_ENV, "7".to_owned()),
        (PAD_ENV, "1500".to_owned()),
    ];
    envs.extend(extra.iter().cloned());
    Peer::spawn(path, "write", &envs)
}

/// Checks that the file, reopened alone, holds every commit each writer reported.
fn holds_every_reported_commit(path: &Path, reported: &BTreeMap<i64, i64>, what: &str) {
    let alone = Database::open(path).unwrap();
    let held = commits(&alone);
    for (writer, count) in reported {
        let found = held.get(writer).copied().unwrap_or(0);
        assert!(
            found >= *count,
            "{what}: writer {writer} reported {count} commits but the file holds {found}"
        );
    }
    alone.check().unwrap();
}

// ast-grep-ignore
// The main evidence of phase 4: writers die at random points of commits, tail hand-overs, and checkpoints, and no reported commit is lost.
#[test]
fn writers_killed_at_random_points_lose_no_reported_commit_and_readers_read_whole_prefixes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph.rhizodb");
    drop(writer_options().open(&path).unwrap());
    let mut seed = seed();
    let mut reported: BTreeMap<i64, i64> = BTreeMap::new();
    let mut writers: Vec<(i64, Peer)> = (0..3)
        .map(|tag| (tag, spawn_writer(&path, tag, &[]).0))
        .collect();
    let mut readers = Vec::new();
    for life in 0..12 {
        for (_, writer) in &mut writers {
            writer.wait_for_commits(1 + (next(&mut seed) % 6) as usize);
        }
        std::thread::sleep(Duration::from_millis(next(&mut seed) % 40));
        let victim = (next(&mut seed) % writers.len() as u64) as usize;
        let (tag, writer) = writers.swap_remove(victim);
        let committed = writer.kill();
        if let Some(last) = committed.last() {
            reported.insert(tag, last + 1);
        }
        let (respawned, start) = spawn_writer(&path, tag, &[]);
        let known = reported.get(&tag).copied().unwrap_or(0);
        assert!(
            start >= known,
            "life {life}: writer {tag} reported {known} commits, but its next life starts at {start}"
        );
        writers.push((tag, respawned));
        let (reader, seen) = Peer::spawn(&path, "read", &[]);
        let sum: i64 = reported.values().sum();
        assert!(seen >= sum, "life {life}: a reader read {seen} of {sum}");
        readers.push((reader, seen));
        if readers.len() > 2 {
            let victim = (next(&mut seed) % readers.len() as u64) as usize;
            readers.swap_remove(victim).0.kill();
        }
    }
    for (tag, writer) in writers {
        if let Some(last) = writer.kill().last() {
            reported.insert(tag, last + 1);
        }
    }
    let sum: i64 = reported.values().sum();
    for (reader, seen) in readers {
        let still = reader.close_reader();
        assert!(still >= seen.max(sum), "a reader read {still} of {sum}");
    }
    holds_every_reported_commit(&path, &reported, "after every life");
}

// ast-grep-ignore
// Each named step of a commit, a tail hand-over, and a checkpoint, at its first and a later hit, in a writer beside another writer and a reader.
#[cfg(crash_points)]
#[test]
fn a_writer_killed_at_each_step_of_a_commit_a_tail_hand_over_or_a_checkpoint_loses_no_reported_commit()
 {
    const POINTS: [(&str, &[u64]); 11] = [
        ("write-lock-taken-at-open", &[1]),
        ("commit-durable", &[1, 5]),
        ("tail-published", &[1, 5]),
        ("tail-taken-over", &[1, 5]),
        ("segment-started", &[1, 3]),
        ("checkpoint-lock-taken", &[1, 3]),
        ("stage-written", &[1, 3]),
        ("header-written", &[1, 3]),
        ("header-published", &[1, 3]),
        ("segments-removed", &[1, 3]),
        ("compacted", &[1, 3]),
    ];
    let tuning = ("RHIZODB_CRASH_TUNING", "1".to_owned());
    let dying_writer = |point: &str, hit: u64| {
        vec![
            (TAG_ENV, "0".to_owned()),
            (COMPACT_ENV, "3".to_owned()),
            (PAD_ENV, "6000".to_owned()),
            tuning.clone(),
            ("RHIZODB_CRASH_POINT", format!("{point}:{hit}")),
        ]
    };
    for (point, hits) in POINTS {
        for hit in hits {
            let what = format!("{point} at hit {hit}");
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("graph.rhizodb");
            drop(writer_options().open(&path).unwrap());
            let (mut steady, _) = spawn_writer(&path, 1, std::slice::from_ref(&tuning));
            steady.wait_for_commits(3);
            let dying =
                Peer::start(&path, "write", &dying_writer(point, *hit)).wait_for_abort(point);
            let mut reported = BTreeMap::new();
            if let Some(last) = dying.last() {
                reported.insert(0, last + 1);
            }
            steady.wait_for_commits(5);
            let (reader, seen) = Peer::spawn(&path, "read", &[]);
            let (mut after, start) = spawn_writer(&path, 0, std::slice::from_ref(&tuning));
            assert!(
                start >= reported.get(&0).copied().unwrap_or(0),
                "{what}: the next writer starts at {start} after {dying:?}"
            );
            after.wait_for_commits(4);
            steady.wait_for_commits(2);
            if let Some(last) = after.kill().last() {
                reported.insert(0, last + 1);
            }
            if let Some(last) = steady.kill().last() {
                reported.insert(1, last + 1);
            }
            let sum: i64 = reported.values().sum();
            let still = reader.close_reader();
            assert!(
                still >= seen.max(sum),
                "{what}: a reader read {still} of {sum}"
            );
            holds_every_reported_commit(&path, &reported, &what);
        }
    }
}

// ast-grep-ignore
// On Windows a removed segment keeps its name while any process holds it open, so a writer that joins after a recovery still finds a segment of the log before it.
#[test]
fn a_writer_joins_after_a_recovery_while_a_segment_of_the_older_log_is_held_open() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph.rhizodb");
    drop(writer_options().open(&path).unwrap());
    let (mut dying, _) = spawn_writer(&path, 0, &[]);
    dying.wait_for_commits(20);
    let mut reported = BTreeMap::new();
    reported.insert(0, dying.kill().last().map_or(0, |last| last + 1));
    let segments: Vec<std::fs::File> = std::fs::read_dir(directory.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|name| name.to_string_lossy().contains("-wal-"))
        .map(|name| std::fs::File::open(name).unwrap())
        .collect();
    assert!(!segments.is_empty(), "the killed writer left no log");
    let (mut recovering, start) = spawn_writer(&path, 1, &[]);
    assert_eq!(start, 0);
    recovering.wait_for_commits(5);
    let joined = writer_options().open(&path).unwrap();
    run(&joined, "INSERT (:Commit {w: 2, n: 0, pad: 'joined'})");
    assert!(commits(&joined).get(&0).copied().unwrap_or(0) >= reported[&0]);
    drop(joined);
    reported.insert(1, recovering.kill().last().map_or(0, |last| last + 1));
    reported.insert(2, 1);
    drop(segments);
    holds_every_reported_commit(&path, &reported, "after a join beside an older log");
}
