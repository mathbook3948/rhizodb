use std::fs;
use std::io;

use super::publish;
use crate::error::ErrorKind;

fn no_links(_: &std::path::Path, _: &std::path::Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::PermissionDenied,
        "no hard links here",
    ))
}

#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
#[test]
fn a_file_system_that_cannot_link_gets_the_backup_moved_into_place() {
    let directory = tempfile::tempdir().unwrap();
    let staging = directory.path().join(".backup.staging");
    let target = directory.path().join("backup.rhizodb");
    fs::write(&staging, b"copy").unwrap();
    publish(&staging, &target, no_links, super::rename_without_replacing).unwrap();
    assert_eq!(fs::read(&target).unwrap(), b"copy");
    assert!(!staging.exists());
}

#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
#[test]
fn a_move_into_place_keeps_a_file_created_at_the_target_first() {
    let directory = tempfile::tempdir().unwrap();
    let staging = directory.path().join(".backup.staging");
    let target = directory.path().join("backup.rhizodb");
    fs::write(&staging, b"copy").unwrap();
    let error = publish(&staging, &target, no_links, |from, to| {
        fs::write(to, b"theirs")?;
        super::rename_without_replacing(from, to)
    })
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::BackupTargetExists);
    assert_eq!(fs::read(&target).unwrap(), b"theirs");
}

#[test]
fn a_file_system_that_can_neither_link_nor_move_fails_the_backup_and_leaves_the_target_alone() {
    let directory = tempfile::tempdir().unwrap();
    let staging = directory.path().join(".backup.staging");
    let target = directory.path().join("backup.rhizodb");
    fs::write(&staging, b"copy").unwrap();
    let error = publish(&staging, &target, no_links, |_, _| {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "no renames here",
        ))
    })
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Storage);
    let (context, io) = error.io_cause().unwrap();
    assert!(context.contains("no hard links here"), "{context}");
    assert!(
        context.contains("the file system may not support hard links"),
        "{context}"
    );
    assert_eq!(io.kind(), io::ErrorKind::Unsupported);
    assert!(!target.exists());
    assert_eq!(fs::read(&staging).unwrap(), b"copy");
}

#[test]
fn a_file_created_at_the_target_before_the_link_is_kept() {
    let directory = tempfile::tempdir().unwrap();
    let staging = directory.path().join(".backup.staging");
    let target = directory.path().join("backup.rhizodb");
    fs::write(&staging, b"copy").unwrap();
    let error = publish(
        &staging,
        &target,
        |from, to| {
            fs::write(to, b"theirs")?;
            fs::hard_link(from, to)
        },
        |_, _| panic!("a link that found the target taken is not retried as a move"),
    )
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::BackupTargetExists);
    assert_eq!(fs::read(&target).unwrap(), b"theirs");
}
