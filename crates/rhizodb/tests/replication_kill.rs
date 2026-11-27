//! Kills a child process that applies a writer's changes to a replica on disk at varied points,
//! then reopens the replica and checks that it is at a position the writer passed with the graph
//! the writer held there, and that applying the same changes again reaches the writer.

use std::collections::HashMap;
use std::fs;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::path::Path;
use std::process::Child;
use std::process::ChildStdout;
use std::process::Command;
use std::process::Stdio;
use std::thread::sleep;
use std::time::Duration;

use rhizodb::Changes;
use rhizodb::Database;
use rhizodb::IndexTarget;
use rhizodb::OpenOptions;
use rhizodb::Parameters;
use rhizodb::Position;
use rhizodb::Value;

const DIRECTORY_ENV: &str = "RHIZODB_REPLICA_KILL_DIRECTORY";
const FAST_TRIALS: u32 = 10;

fn trials() -> u32 {
    let scale = std::env::var("RHIZODB_TEST_SCALE")
        .ok()
        .and_then(|scale| scale.parse::<u32>().ok())
        .unwrap_or(1);
    FAST_TRIALS * scale.max(1)
}

fn export(db: &Database) -> Vec<u8> {
    let mut out = Vec::new();
    db.export(&mut out).unwrap();
    out
}

fn batch_path(directory: &Path, index: usize) -> std::path::PathBuf {
    directory.join(format!("changes-{index:06}"))
}

fn replica_path(directory: &Path) -> std::path::PathBuf {
    directory.join("replica.rhizodb")
}

fn write_changes(directory: &Path) -> (Position, HashMap<Position, Vec<u8>>, usize) {
    let writer = Database::in_memory().unwrap();
    writer.promote(1, b"writer").unwrap();
    writer
        .create_property_index("by_i", IndexTarget::Label("Item"), "i")
        .unwrap();
    for i in 0..40_i64 {
        let parameters = Parameters::from([
            ("i".to_owned(), Value::Integer(i)),
            ("payload".to_owned(), Value::String("p".repeat(2_000))),
        ]);
        writer
            .execute(
                "INSERT (:Item {i: $i, payload: $payload})-[:NEXT]->(:Item {i: $i + 1000})",
                &parameters,
            )
            .unwrap();
        if i % 9 == 4 {
            writer
                .execute(
                    "MATCH (n:Item) WHERE n.i % 4 = $i % 4 SET n.payload = 'short'",
                    &parameters,
                )
                .unwrap();
        }
    }
    let shadow = OpenOptions::new().replica(true).in_memory().unwrap();
    let mut seen = HashMap::new();
    seen.insert(shadow.position(), export(&shadow));
    let mut count = 0;
    loop {
        let changes = writer.changes_since(&shadow.position(), 1).unwrap();
        if changes.is_empty() {
            break;
        }
        fs::write(batch_path(directory, count), changes.as_bytes()).unwrap();
        shadow.apply(&changes).unwrap();
        seen.insert(shadow.position(), export(&shadow));
        count += 1;
    }
    assert_eq!(shadow.position(), writer.position());
    (writer.position(), seen, count)
}

#[test]
#[ignore = "run only as the child of a_replica_killed_during_apply_reopens_at_a_position_its_writer_passed"]
fn replica_kill_child() {
    let Ok(directory) = std::env::var(DIRECTORY_ENV) else {
        return;
    };
    let directory = Path::new(&directory);
    let replica = OpenOptions::new()
        .replica(true)
        .open(replica_path(directory))
        .unwrap();
    let mut out = std::io::stdout();
    writeln!(out, "open").unwrap();
    let mut index = 0;
    while let Ok(bytes) = fs::read(batch_path(directory, index)) {
        let changes = Changes::from_bytes(&bytes).unwrap();
        index += 1;
        if changes.last().lsn() <= replica.position().lsn() {
            continue;
        }
        let position = replica.apply(&changes).unwrap();
        writeln!(out, "applied {}", position.lsn()).unwrap();
        out.flush().unwrap();
    }
    writeln!(out, "done").unwrap();
    out.flush().unwrap();
    loop {
        sleep(Duration::from_secs(1));
    }
}

struct ChildReplica {
    child: Child,
    lines: BufReader<ChildStdout>,
}

impl ChildReplica {
    fn spawn(directory: &Path) -> Self {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["replica_kill_child", "--exact", "--ignored", "--nocapture"])
            .env(DIRECTORY_ENV, directory)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let lines = BufReader::new(child.stdout.take().unwrap());
        Self { child, lines }
    }

    fn next_line(&mut self) -> Option<String> {
        let mut line = String::new();
        match self.lines.read_line(&mut line) {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(line.trim().to_owned()),
        }
    }

    fn run_until(&mut self, applied: usize) -> bool {
        let mut seen = 0;
        while let Some(line) = self.next_line() {
            if line == "done" {
                return true;
            }
            if line.starts_with("applied") {
                seen += 1;
                if seen >= applied {
                    return false;
                }
            }
        }
        panic!("the child replica stopped without reporting done");
    }
}

impl Drop for ChildReplica {
    fn drop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
    }
}

#[test]
#[ignore = "left out of mise run check, run by mise run random"]
fn a_replica_killed_during_apply_reopens_at_a_position_its_writer_passed() {
    let directory = tempfile::tempdir().unwrap();
    let (last, seen, count) = write_changes(directory.path());
    let mut kills = 0;
    for trial in 0..trials() {
        let replica = replica_path(directory.path());
        for name in fs::read_dir(directory.path()).unwrap() {
            let name = name.unwrap().path();
            if name
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("replica"))
            {
                fs::remove_file(name).unwrap();
            }
        }
        let mut done = false;
        let mut round = 0;
        while !done {
            let mut child = ChildReplica::spawn(directory.path());
            let stop = 1 + (trial as usize * 7 + round * 13) % (count / 3).max(1);
            done = child.run_until(stop);
            if !done {
                sleep(Duration::from_micros(u64::from(trial) * 37 % 900));
                drop(child);
                kills += 1;
                let reopened = OpenOptions::new().replica(true).open(&replica).unwrap();
                let position = reopened.position();
                let expected = seen
                    .get(&position)
                    .unwrap_or_else(|| panic!("trial {trial} reopened at {position:?}"));
                assert!(
                    &export(&reopened) == expected,
                    "trial {trial} at {position:?}"
                );
                reopened.check().unwrap();
            }
            round += 1;
        }
        let reopened = OpenOptions::new().replica(true).open(&replica).unwrap();
        assert_eq!(reopened.position(), last, "trial {trial}");
        assert!(export(&reopened) == seen[&last], "trial {trial}");
    }
    assert!(kills > 0);
}
