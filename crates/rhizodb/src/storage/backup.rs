use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use super::Storage;
use super::StorageError;
use crate::error::ErrorKind;

static STAGED: AtomicU64 = AtomicU64::new(0);

/// Copies the committed state of `storage`, as one read snapshot sees it, into a new database
/// file at `path`. The copy is written under a staging name in the same directory, synced, and
/// linked into place, so `path` holds either nothing or the whole copy, and an existing file at
/// `path` is never replaced.
pub(crate) fn write<S: Storage>(storage: &S, path: &Path) -> Result<(), StorageError> {
    if exists(path).map_err(StorageError::new)? {
        return Err(target_exists(path));
    }
    let staging = staging_path(path).map_err(StorageError::new)?;
    let copied = storage
        .begin_read()
        .and_then(|snapshot| S::write_copy(&snapshot, &staging));
    let published = copied.and_then(|()| {
        publish(&staging, path).map_err(|error| match error.kind() {
            io::ErrorKind::AlreadyExists => target_exists(path),
            _ => StorageError::new(error),
        })
    });
    if published.is_err() {
        let _ = fs::remove_file(&staging);
    }
    published
}

fn exists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn target_exists(path: &Path) -> StorageError {
    StorageError::of_kind(ErrorKind::BackupTargetExists, already_exists(path))
}

fn already_exists(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("the backup target {} already exists", path.display()),
    )
}

fn staging_path(path: &Path) -> io::Result<PathBuf> {
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "the backup path does not name a file",
        )
    })?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    let mut staged = OsString::from(".");
    staged.push(name);
    staged.push(format!(
        ".backup-{}-{nanos}-{}",
        std::process::id(),
        STAGED.fetch_add(1, Ordering::Relaxed)
    ));
    Ok(path.with_file_name(staged))
}

fn publish(staging: &Path, path: &Path) -> io::Result<()> {
    fs::OpenOptions::new()
        .write(true)
        .open(staging)?
        .sync_all()?;
    match fs::hard_link(staging, path) {
        Ok(()) => fs::remove_file(staging)?,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            return Err(already_exists(path));
        }
        Err(_) if !exists(path)? => fs::rename(staging, path)?,
        Err(_) => return Err(already_exists(path)),
    }
    sync_directory(path)
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    let directory = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    fs::File::open(directory)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}
