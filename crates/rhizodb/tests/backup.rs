//! Kills a child process that writes and backs its database up in turn, at varied points, and
//! checks that each backup file it left opens and holds one whole commit, and that the files a
//! killed backup left behind are only its staging files.

mod support;

use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

use rhizodb::Database;
use rhizodb::IndexTarget;
use rhizodb::Parameters;
use rhizodb::Value;
use support::*;

const DIRECTORY_ENV: &str = "RHIZODB_BACKUP_DIRECTORY";
const FAST_TRIALS: u64 = 6;

fn tick(db: &Database, i: i64) {
    let parameters = Parameters::from([("i".to_owned(), Value::Integer(i))]);
    let mut tx = db.begin_write().unwrap();
    tx.execute(
        "UNWIND range(1, 200) AS j INSERT (:Tick {i: $i, j: j, text: 'tick ' + toString($i) + \
         ' ' + toString(j), pad: 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'})",
        &parameters,
    )
    .unwrap();
    tx.execute("MATCH (c:Counter) SET c.n = $i", &parameters)
        .unwrap();
    tx.commit().unwrap();
}

#[test]
#[ignore = "child process of the backup kill test, selected by RHIZODB_BACKUP_DIRECTORY"]
fn backup_child() {
    let Ok(directory) = std::env::var(DIRECTORY_ENV) else {
        return;
    };
    let directory = Path::new(&directory);
    let db = Database::open(directory.join("source")).unwrap();
    db.create_keyword_index("tick_text", IndexTarget::Label("Tick"), &["text"])
        .unwrap();
    run(&db, "INSERT (:Counter {n: 0})");
    let mut stdout = std::io::stdout();
    for i in 1..i64::MAX {
        tick(&db, i);
        db.backup_to(directory.join(format!("backup-{i}"))).unwrap();
        writeln!(stdout, "backed up {i}").unwrap();
        stdout.flush().unwrap();
    }
}

fn check_backup(path: &Path) {
    let db = Database::open(path).unwrap();
    let n = match single(&db, "MATCH (c:Counter) RETURN c.n") {
        Value::Integer(n) => n,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        single(&db, "MATCH (t:Tick) RETURN count(t)"),
        Value::Integer(n * 200)
    );
    assert_eq!(
        single(
            &db,
            "CALL db.index.keyword.queryNodes('tick_text', 'tick', 100000000) YIELD node \
             RETURN count(node)"
        ),
        Value::Integer(n * 200)
    );
    run(&db, "INSERT (:Tick {i: -1})");
}

#[test]
fn a_killed_backup_leaves_its_target_absent_or_whole() {
    for trial in 0..cases(FAST_TRIALS) {
        let directory = tempfile::tempdir().unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["backup_child", "--exact", "--ignored", "--nocapture"])
            .env(DIRECTORY_ENV, directory.path())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let wanted = 1 + trial % 4;
        let mut reported = Vec::new();
        let mut line = String::new();
        while reported.len() < wanted as usize {
            line.clear();
            assert!(stdout.read_line(&mut line).unwrap() > 0, "the child exited");
            if let Some(i) = line.trim_end().strip_prefix("backed up ") {
                reported.push(i.parse::<u64>().unwrap());
            }
        }
        let started = Instant::now();
        std::thread::sleep(Duration::from_micros(1 + trial * 7919 % 40_000));
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(started.elapsed() < Duration::from_secs(60));
        for i in &reported {
            assert!(directory.path().join(format!("backup-{i}")).is_file());
        }
        for entry in std::fs::read_dir(directory.path()).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().into_owned();
            if name.starts_with("backup-") {
                check_backup(&directory.path().join(&name));
            } else {
                assert!(
                    name == "source"
                        || name.starts_with(".source")
                        || name.starts_with("source-wal-")
                        || name.contains(".backup-"),
                    "unexpected file {name}"
                );
            }
        }
    }
}
