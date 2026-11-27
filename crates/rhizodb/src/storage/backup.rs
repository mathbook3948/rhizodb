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
/// linked into place, or moved there where the file system has no hard links, so `path` holds
/// either nothing or the whole copy, and an existing file at `path` is never replaced.
pub(crate) fn write<S: Storage>(storage: &S, path: &Path) -> Result<(), StorageError> {
    let checked = exists(path).map_err(|error| {
        StorageError::io(
            ErrorKind::Storage,
            format!("cannot check whether {} exists", path.display()),
            error,
        )
    });
    if checked? {
        return Err(target_exists(path));
    }
    let staging = staging_path(path).map_err(|error| {
        StorageError::io(
            ErrorKind::Storage,
            format!("cannot back up to {}", path.display()),
            error,
        )
    })?;
    let copied = storage
        .begin_read()
        .and_then(|snapshot| S::write_copy(&snapshot, &staging))
        .map_err(|error| {
            error.in_context(|| format!("cannot write the backup to {}", path.display()))
        });
    let published = copied.and_then(|()| {
        publish(
            &staging,
            path,
            |from, to| fs::hard_link(from, to),
            rename_without_replacing,
        )
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
    StorageError::of_kind(
        ErrorKind::BackupTargetExists,
        io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("the backup target {} already exists", path.display()),
        ),
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

fn publish(
    staging: &Path,
    path: &Path,
    link: impl FnOnce(&Path, &Path) -> io::Result<()>,
    rename: impl FnOnce(&Path, &Path) -> io::Result<()>,
) -> Result<(), StorageError> {
    let failed = |what: &str, error| {
        StorageError::io(
            ErrorKind::Storage,
            format!("cannot {what} {}", path.display()),
            error,
        )
    };
    fs::OpenOptions::new()
        .write(true)
        .open(staging)
        .and_then(|file| file.sync_all())
        .map_err(|error| failed("sync the backup for", error))?;
    match link(staging, path) {
        Ok(()) => fs::remove_file(staging)
            .map_err(|error| failed("remove the staging file of the backup at", error))?,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            return Err(target_exists(path));
        }
        Err(link_error) => rename(staging, path).map_err(|error| {
            if error.kind() == io::ErrorKind::AlreadyExists {
                return target_exists(path);
            }
            StorageError::io(
                ErrorKind::Storage,
                format!(
                    "cannot hard-link the backup to {} ({link_error}), the file system may not \
                     support hard links, and moving it there without replacing a file failed",
                    path.display()
                ),
                error,
            )
        })?,
    }
    sync_directory(path).map_err(|error| failed("sync the directory of", error))
}

#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
fn rename_without_replacing(from: &Path, to: &Path) -> io::Result<()> {
    use rustix::fs::CWD;
    use rustix::fs::RenameFlags;
    Ok(rustix::fs::renameat_with(
        CWD,
        from,
        CWD,
        to,
        RenameFlags::NOREPLACE,
    )?)
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
fn rename_without_replacing(_from: &Path, _to: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "this platform cannot rename a file without replacing one",
    ))
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

#[cfg(test)]
#[path = "backup.test.rs"]
mod tests;
