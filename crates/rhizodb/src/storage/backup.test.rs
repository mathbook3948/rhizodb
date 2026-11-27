use std::fs;
use std::io;

use super::publish;

#[test]
fn a_file_system_that_cannot_link_fails_the_backup_and_leaves_the_target_alone() {
    let directory = tempfile::tempdir().unwrap();
    let staging = directory.path().join(".backup.staging");
    let target = directory.path().join("backup.rhizodb");
    fs::write(&staging, b"copy").unwrap();
    let error = publish(&staging, &target, |_, _| {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "no hard links here",
        ))
    })
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::Unsupported);
    assert!(error.to_string().contains("no hard links here"), "{error}");
    assert!(!target.exists());
    assert_eq!(fs::read(&staging).unwrap(), b"copy");
}

#[test]
fn a_file_created_at_the_target_before_the_link_is_kept() {
    let directory = tempfile::tempdir().unwrap();
    let staging = directory.path().join(".backup.staging");
    let target = directory.path().join("backup.rhizodb");
    fs::write(&staging, b"copy").unwrap();
    let error = publish(&staging, &target, |from, to| {
        fs::write(to, b"theirs")?;
        fs::hard_link(from, to)
    })
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(fs::read(&target).unwrap(), b"theirs");
}
