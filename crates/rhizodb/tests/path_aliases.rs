//! Opens one database file through paths that name it differently (a symlinked directory, a
//! symlink to the file, `..`, a relative path) and checks that every open sees the same log
//! segments and counts as the same file, and that a file with a second hard link is refused.
#![cfg(unix)]

mod support;

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;

use rhizodb::Database;
use rhizodb::ErrorKind;
use rhizodb::OpenOptions;
use support::*;

const DATABASE_ENV: &str = "RHIZODB_PATH_ALIAS_DATABASE";

fn count(db: &Database) -> i64 {
    ints(&run(db, "MATCH (n:Item) RETURN count(n)"))[0]
}

#[test]
#[ignore = "child process of the path alias tests, selected by RHIZODB_PATH_ALIAS_DATABASE"]
fn path_alias_child() {
    let Ok(path) = std::env::var(DATABASE_ENV) else {
        return;
    };
    let db = Database::open(&path).unwrap();
    run(&db, "UNWIND range(101, 150) AS i INSERT (:Item {i: i})");
    std::process::exit(0);
}

/// Commits 100 items through `real`, then 50 more through `alias` in a child process that exits
/// without closing, so the last commits live only in log segments.
fn crash_through(real: &Path, alias: &Path) {
    let db = Database::open(real).unwrap();
    run(&db, "UNWIND range(1, 100) AS i INSERT (:Item {i: i})");
    drop(db);
    let status = Command::new(std::env::current_exe().unwrap())
        .args(["path_alias_child", "--exact", "--ignored", "--nocapture"])
        .env(DATABASE_ENV, alias)
        .stdout(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
}

fn assert_every_path_sees_every_commit(paths: &[&Path]) {
    for path in paths {
        let db = OpenOptions::new().read_only(true).open(path).unwrap();
        assert_eq!(count(&db), 150, "read-only through {}", path.display());
    }
    for path in paths {
        let db = Database::open(path).unwrap();
        assert_eq!(count(&db), 150, "through {}", path.display());
        db.check().unwrap();
    }
}

/// `path` as a path relative to the current directory, climbing to the root with `..`.
fn relative_to_current(path: &Path) -> PathBuf {
    let depth = std::env::current_dir().unwrap().components().count() - 1;
    let mut relative: PathBuf = std::iter::repeat_n("..", depth).collect();
    relative.push(path.strip_prefix("/").unwrap());
    relative
}

struct Aliases {
    _directory: tempfile::TempDir,
    real: PathBuf,
    linked_directory: PathBuf,
    linked_file: PathBuf,
    dotted: PathBuf,
    relative: PathBuf,
}

fn aliases() -> Aliases {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("data");
    std::fs::create_dir(&data).unwrap();
    let real = data.join("graph.rhizodb");
    let links = directory.path().join("links");
    std::fs::create_dir(&links).unwrap();
    std::os::unix::fs::symlink(&data, directory.path().join("linked")).unwrap();
    drop(Database::open(&real).unwrap());
    let linked_file = links.join("alias.rhizodb");
    std::os::unix::fs::symlink(&real, &linked_file).unwrap();
    Aliases {
        linked_directory: directory.path().join("linked/graph.rhizodb"),
        dotted: data.join("../data/graph.rhizodb"),
        relative: relative_to_current(&real),
        _directory: directory,
        real,
        linked_file,
    }
}

#[test]
fn commits_through_a_symlinked_directory_survive_a_crash_for_every_path() {
    let a = aliases();
    crash_through(&a.real, &a.linked_directory);
    assert_every_path_sees_every_commit(&[&a.real, &a.linked_directory, &a.dotted]);
}

#[test]
fn commits_through_a_dotted_path_survive_a_crash_for_every_path() {
    let a = aliases();
    crash_through(&a.real, &a.dotted);
    assert_every_path_sees_every_commit(&[&a.real, &a.linked_directory, &a.dotted]);
}

#[test]
fn commits_through_a_symlink_to_the_file_survive_a_crash_for_every_path() {
    let a = aliases();
    crash_through(&a.real, &a.linked_file);
    assert_every_path_sees_every_commit(&[&a.real, &a.linked_file]);
}

#[test]
fn commits_through_the_real_path_survive_a_crash_for_a_symlink_to_the_file() {
    let a = aliases();
    crash_through(&a.real, &a.real);
    assert_every_path_sees_every_commit(&[&a.linked_file, &a.real]);
}

#[test]
fn commits_through_a_relative_path_survive_a_crash_for_every_path() {
    let a = aliases();
    crash_through(&a.real, &a.relative);
    assert_every_path_sees_every_commit(&[&a.real, &a.linked_file, &a.relative]);
}

#[test]
fn one_process_opens_a_file_once_whatever_path_names_it() {
    let a = aliases();
    let read_only = OpenOptions::new().read_only(true);
    for alias in [&a.linked_directory, &a.dotted, &a.linked_file, &a.relative] {
        for (first, second) in [(&a.real, alias), (alias, &a.real)] {
            let held = Database::open(first).unwrap();
            let error = Database::open(second).map(drop).expect_err(&format!(
                "{} then {}",
                first.display(),
                second.display()
            ));
            assert_eq!(error.kind(), ErrorKind::DatabaseAlreadyOpen, "{error}");
            drop(held);
            let held = read_only.open(first).unwrap();
            let error = read_only.open(second).map(drop).expect_err(&format!(
                "{} then {}",
                first.display(),
                second.display()
            ));
            assert_eq!(error.kind(), ErrorKind::DatabaseAlreadyOpen, "{error}");
            drop(held);
        }
    }
}

#[test]
fn a_file_with_a_second_hard_link_is_refused_by_either_name_until_the_link_is_gone() {
    let a = aliases();
    crash_through(&a.real, &a.real);
    let hard_link = a.real.with_file_name("hard.rhizodb");
    std::fs::hard_link(&a.real, &hard_link).unwrap();
    let read_only = OpenOptions::new().read_only(true);
    for path in [&a.real, &hard_link, &a.linked_file] {
        for opened in [Database::open(path), read_only.open(path)] {
            let error = opened.map(drop).expect_err(&path.display().to_string());
            assert_eq!(error.kind(), ErrorKind::Storage, "{error}");
            let mut causes = String::new();
            let mut cause: Option<&dyn std::error::Error> = Some(&error);
            while let Some(next) = cause {
                causes.push_str(&next.to_string());
                cause = next.source();
            }
            assert!(causes.contains("hard links"), "{causes}");
        }
    }
    std::fs::remove_file(&hard_link).unwrap();
    assert_every_path_sees_every_commit(&[&a.real, &a.linked_file]);
}
