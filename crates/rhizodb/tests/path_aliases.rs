//! Opens one database file through paths that name it differently (a symlinked directory, a
//! symlink to the file, `..`, a relative path) and checks that every open sees the same log
//! segments and counts as the same file, that a file with a second hard link is refused, and
//! that the log an older release named after a symlink to the file is moved beside the file.
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
const PAYLOAD_ENV: &str = "RHIZODB_PATH_ALIAS_PAYLOAD";

fn count(db: &Database) -> i64 {
    ints(&run(db, "MATCH (n:Item) RETURN count(n)"))[0]
}

#[test]
#[ignore = "child process of the path alias tests, selected by RHIZODB_PATH_ALIAS_DATABASE"]
fn path_alias_child() {
    let Ok(path) = std::env::var(DATABASE_ENV) else {
        return;
    };
    let payload = std::env::var(PAYLOAD_ENV).map_or(0, |bytes| bytes.parse().unwrap());
    let db = Database::open(&path).unwrap();
    for i in 101..=150 {
        db.execute(
            "INSERT (:Item {i: $i, payload: $payload})",
            &[
                ("i".to_owned(), rhizodb::Value::Integer(i)),
                (
                    "payload".to_owned(),
                    rhizodb::Value::String("x".repeat(payload)),
                ),
            ]
            .into_iter()
            .collect(),
        )
        .unwrap();
    }
    std::process::exit(0);
}

/// Commits 100 items through `real`, then 50 more through `alias` in a child process that exits
/// without closing, so the last commits live only in log segments.
fn crash_through(real: &Path, alias: &Path) {
    crash_with_payload(real, alias, 0);
}

fn crash_with_payload(real: &Path, alias: &Path, payload: usize) {
    let db = Database::open(real).unwrap();
    run(&db, "UNWIND range(1, 100) AS i INSERT (:Item {i: i})");
    drop(db);
    let status = Command::new(std::env::current_exe().unwrap())
        .args(["path_alias_child", "--exact", "--ignored", "--nocapture"])
        .env(DATABASE_ENV, alias)
        .env(PAYLOAD_ENV, payload.to_string())
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

/// The log segments beside `file`, under its name, oldest first.
fn segments(file: &Path) -> Vec<PathBuf> {
    let prefix = format!("{}-wal-", file.file_name().unwrap().to_str().unwrap());
    let mut found: Vec<PathBuf> = std::fs::read_dir(file.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with(&prefix)
        })
        .collect();
    found.sort();
    found
}

/// Moves each of `segments` beside `file`, under its name, keeping its number.
fn move_segments(segments: &[PathBuf], file: &Path) {
    let to = file.file_name().unwrap().to_str().unwrap();
    for segment in segments {
        let name = segment.file_name().unwrap().to_str().unwrap();
        let (_, sequence) = name.rsplit_once("-wal-").unwrap();
        std::fs::rename(segment, file.with_file_name(format!("{to}-wal-{sequence}"))).unwrap();
    }
}

fn causes(error: &rhizodb::Error) -> String {
    let mut causes = String::new();
    let mut cause: Option<&dyn std::error::Error> = Some(error);
    while let Some(next) = cause {
        causes.push_str(&next.to_string());
        cause = next.source();
    }
    causes
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
    relative_link: PathBuf,
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
    let relative_link = links.join("relative.rhizodb");
    std::os::unix::fs::symlink("../linked/../data/graph.rhizodb", &relative_link).unwrap();
    Aliases {
        linked_directory: directory.path().join("linked/graph.rhizodb"),
        dotted: data.join("../data/graph.rhizodb"),
        relative: relative_to_current(&real),
        _directory: directory,
        real,
        linked_file,
        relative_link,
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
    assert_every_path_sees_every_commit(&[&a.real, &a.linked_file, &a.relative_link]);
}

#[test]
fn commits_through_a_relative_symlink_to_the_file_survive_a_crash_for_every_path() {
    let a = aliases();
    crash_through(&a.real, &a.relative_link);
    assert_every_path_sees_every_commit(&[&a.real, &a.linked_file, &a.relative_link]);
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
    for alias in [
        &a.linked_directory,
        &a.dotted,
        &a.linked_file,
        &a.relative_link,
        &a.relative,
    ] {
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
            assert!(causes(&error).contains("hard links"), "{error}");
        }
    }
    std::fs::remove_file(&hard_link).unwrap();
    assert_every_path_sees_every_commit(&[&a.real, &a.linked_file]);
}

#[test]
fn one_process_holds_no_two_read_only_opens_through_two_hard_links() {
    let a = aliases();
    let hard_link = a.real.with_file_name("hard.rhizodb");
    std::fs::hard_link(&a.real, &hard_link).unwrap();
    let read_only = OpenOptions::new().read_only(true);
    let first = read_only.open(&a.real);
    let second = read_only.open(&hard_link);
    assert!(
        first.is_err() || second.is_err(),
        "two read-only opens of one file in one process"
    );
}

/// Crashes a writer through the real path, then moves its log beside the symlink to the file,
/// where a release that named the log after the path it was given left it.
fn older_release_layout(a: &Aliases, payload: usize) -> Vec<PathBuf> {
    crash_with_payload(&a.real, &a.real, payload);
    let own = segments(&a.real);
    assert!(!own.is_empty());
    move_segments(&own, &a.linked_file);
    segments(&a.linked_file)
}

fn files(directory: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut files: Vec<_> = std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            let bytes = std::fs::read(&path).unwrap_or_default();
            (path, bytes)
        })
        .collect();
    files.sort();
    files
}

#[test]
fn the_log_an_older_release_named_after_a_symlink_moves_beside_the_file_on_a_writable_open() {
    let a = aliases();
    older_release_layout(&a, 0);
    let before = [
        files(a.real.parent().unwrap()),
        files(a.linked_file.parent().unwrap()),
    ];
    let error = OpenOptions::new()
        .read_only(true)
        .open(&a.linked_file)
        .map(drop)
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::ReadOnly, "{error}");
    assert!(causes(&error).contains("older release"), "{error}");
    let after = [
        files(a.real.parent().unwrap()),
        files(a.linked_file.parent().unwrap()),
    ];
    assert_eq!(after, before);
    let db = Database::open(&a.linked_file).unwrap();
    assert_eq!(count(&db), 150);
    drop(db);
    assert!(segments(&a.linked_file).is_empty());
    assert_every_path_sees_every_commit(&[&a.real, &a.linked_file]);
}

fn restore(files: &[(PathBuf, Vec<u8>)], directories: &[&Path]) {
    for directory in directories {
        for entry in std::fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if !std::fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink()
            {
                std::fs::remove_file(path).unwrap();
            }
        }
    }
    for (path, bytes) in files {
        if !std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
            std::fs::write(path, bytes).unwrap();
        }
    }
}

#[test]
fn a_move_of_an_older_release_log_killed_after_any_step_fails_loudly_through_the_real_path_and_finishes_through_the_link()
 {
    let a = aliases();
    let stray = older_release_layout(&a, 1 << 20);
    assert!(stray.len() >= 2, "{stray:?}");
    let directories = [a.real.parent().unwrap(), a.linked_file.parent().unwrap()];
    let crashed: Vec<_> = directories.iter().flat_map(|d| files(d)).collect();
    for moved in 1..stray.len() {
        restore(&crashed, &directories);
        move_segments(&stray[stray.len() - moved..], &a.real);
        let before: Vec<_> = directories.iter().flat_map(|d| files(d)).collect();
        for opened in [
            OpenOptions::new().read_only(true).open(&a.real),
            Database::open(&a.real),
        ] {
            let error = opened.map(drop).expect_err(&format!("{moved} moved"));
            assert_eq!(
                error.kind(),
                ErrorKind::CorruptData,
                "{moved} moved: {error}"
            );
        }
        let error = OpenOptions::new()
            .read_only(true)
            .open(&a.linked_file)
            .map(drop)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::ReadOnly, "{moved} moved: {error}");
        let after: Vec<_> = directories.iter().flat_map(|d| files(d)).collect();
        assert_eq!(after, before, "{moved} moved");
        let db = Database::open(&a.linked_file).unwrap();
        assert_eq!(count(&db), 150, "{moved} moved");
        drop(db);
        assert!(segments(&a.linked_file).is_empty());
        assert_every_path_sees_every_commit(&[&a.real, &a.linked_file]);
    }
}

#[test]
fn a_log_beside_a_symlink_that_belongs_to_another_database_is_refused_by_name_and_left_as_it_is() {
    let a = aliases();
    crash_through(&a.real, &a.real);
    for segment in segments(&a.real) {
        std::fs::remove_file(segment).unwrap();
    }
    let other = tempfile::tempdir().unwrap();
    let foreign = other.path().join("alias.rhizodb");
    crash_through(&foreign, &foreign);
    move_segments(&segments(&foreign), &a.linked_file);
    let stray = segments(&a.linked_file);
    let directories = [a.real.parent().unwrap(), a.linked_file.parent().unwrap()];
    let before: Vec<_> = directories.iter().flat_map(|d| files(d)).collect();
    let error = Database::open(&a.linked_file).map(drop).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Storage, "{error}");
    let causes = causes(&error);
    assert!(causes.contains("none was moved"), "{causes}");
    for segment in &stray {
        assert!(causes.contains(&segment.display().to_string()), "{causes}");
    }
    let after: Vec<_> = directories.iter().flat_map(|d| files(d)).collect();
    assert_eq!(after, before);
}

#[test]
fn a_log_both_beside_the_file_and_beside_a_symlink_to_it_is_refused_and_left_as_it_is() {
    let a = aliases();
    crash_through(&a.real, &a.real);
    for segment in segments(&a.real) {
        let name = segment.file_name().unwrap().to_str().unwrap();
        let (_, sequence) = name.rsplit_once("-wal-").unwrap();
        std::fs::copy(
            &segment,
            a.linked_file
                .with_file_name(format!("alias.rhizodb-wal-{sequence}")),
        )
        .unwrap();
    }
    let before = [
        files(a.real.parent().unwrap()),
        files(a.linked_file.parent().unwrap()),
    ];
    for opened in [
        Database::open(&a.linked_file),
        OpenOptions::new().read_only(true).open(&a.linked_file),
    ] {
        let error = opened.map(drop).unwrap_err();
        assert!(
            matches!(error.kind(), ErrorKind::Storage | ErrorKind::ReadOnly),
            "{error}"
        );
    }
    let error = Database::open(&a.linked_file).map(drop).unwrap_err();
    assert!(causes(&error).contains("stale"), "{error}");
    let after = [
        files(a.real.parent().unwrap()),
        files(a.linked_file.parent().unwrap()),
    ];
    assert_eq!(after, before);
    assert_every_path_sees_every_commit(&[&a.real]);
}

#[test]
fn an_open_through_a_dangling_symlink_creates_its_target_and_keeps_the_link() {
    let a = aliases();
    let target = a.real.with_file_name("new.rhizodb");
    let link = a.linked_file.with_file_name("new.rhizodb");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let db = Database::open(&link).unwrap();
    run(&db, "INSERT (:Item {i: 1})");
    drop(db);
    assert!(
        std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(count(&Database::open(&target).unwrap()), 1);
}
