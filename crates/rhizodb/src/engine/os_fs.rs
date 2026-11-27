use std::collections::BTreeSet;
use std::fs;
use std::fs::File;
use std::fs::TryLockError;
use std::io;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use super::block_file::BlockFile;
use super::block_file::BlockSize;
use super::block_file::FileSystem;

/// The files of one directory of the operating system's file system.
#[derive(Debug)]
pub(crate) struct OsFs {
    directory: PathBuf,
    read_only: Option<Claim>,
}

static READ_ONLY_FILES: Mutex<BTreeSet<PathBuf>> = Mutex::new(BTreeSet::new());

// ast-grep-ignore
// Shared locks of one process never conflict, so a claim keeps it to one open of a file.
#[derive(Debug)]
struct Claim(PathBuf);

impl Claim {
    fn take(path: PathBuf) -> io::Result<Self> {
        let path = fs::canonicalize(path)?;
        let mut held = READ_ONLY_FILES
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if !held.insert(path.clone()) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "the database file is already open read-only in this process",
            ));
        }
        Ok(Self(path))
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        READ_ONLY_FILES
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.0);
    }
}

impl OsFs {
    pub(crate) fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
            read_only: None,
        }
    }

    /// The files of `directory`, opened without write access, so the operating system refuses
    /// every write, and with no create, rename, or remove, for the one open of the database file
    /// `name` that this process may hold at a time.
    pub(crate) fn read_only(directory: impl Into<PathBuf>, name: &str) -> io::Result<Self> {
        let directory = directory.into();
        let claim = Claim::take(directory.join(name))?;
        Ok(Self {
            directory,
            read_only: Some(claim),
        })
    }

    fn writable(&self) -> io::Result<()> {
        if self.read_only.is_some() {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "the database is open read-only",
            ))
        } else {
            Ok(())
        }
    }
}

#[derive(Debug)]
pub(crate) struct OsFile {
    file: File,
    size: BlockSize,
    read_only: bool,
}

const LOCK_PATIENCE: Duration = Duration::from_millis(500);

const LONGEST_LOCK_PAUSE: Duration = Duration::from_millis(10);

fn lock_or_skip_where_unsupported(
    try_lock: impl Fn() -> Result<(), TryLockError>,
) -> io::Result<()> {
    let start = Instant::now();
    let mut pause = Duration::from_micros(100);
    loop {
        match try_lock() {
            Ok(()) => return Ok(()),
            Err(TryLockError::WouldBlock) if start.elapsed() < LOCK_PATIENCE => {
                thread::sleep(pause);
                pause = pause.saturating_mul(2).min(LONGEST_LOCK_PAUSE);
            }
            Err(TryLockError::WouldBlock) => {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "the database file is already open",
                ));
            }
            Err(TryLockError::Error(error)) if error.kind() == io::ErrorKind::Unsupported => {
                return Ok(());
            }
            Err(TryLockError::Error(error)) => return Err(error),
        }
    }
}

impl FileSystem for OsFs {
    type File = OsFile;

    fn create(&self, name: &str, size: BlockSize) -> io::Result<OsFile> {
        self.writable()?;
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(self.directory.join(name))?;
        Ok(OsFile {
            file,
            size,
            read_only: false,
        })
    }

    fn open(&self, name: &str, size: BlockSize) -> io::Result<OsFile> {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(self.read_only.is_none())
            .open(self.directory.join(name))?;
        Ok(OsFile {
            file,
            size,
            read_only: self.read_only.is_some(),
        })
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        self.writable()?;
        fs::rename(self.directory.join(from), self.directory.join(to))
    }

    fn remove(&self, name: &str) -> io::Result<()> {
        self.writable()?;
        fs::remove_file(self.directory.join(name))
    }

    fn names(&self) -> io::Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in fs::read_dir(&self.directory)? {
            let entry = entry?;
            if entry.file_type()?.is_file()
                && let Ok(name) = entry.file_name().into_string()
            {
                names.push(name);
            }
        }
        names.sort();
        Ok(names)
    }

    #[cfg(unix)]
    fn sync_names(&self) -> io::Result<()> {
        File::open(&self.directory)?.sync_all()
    }

    #[cfg(not(unix))]
    fn sync_names(&self) -> io::Result<()> {
        Ok(())
    }
}

impl BlockFile for OsFile {
    fn read(&self, first: u64, buffer: &mut [u8]) -> io::Result<()> {
        self.size.require_whole_blocks(buffer.len())?;
        read_exact_at(&self.file, buffer, self.size.offset(first)?)
    }

    fn write(&self, first: u64, buffer: &[u8]) -> io::Result<()> {
        self.size.require_whole_blocks(buffer.len())?;
        let offset = self.size.offset(first)?;
        offset
            .checked_add(buffer.len() as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "offset out of range"))?;
        write_all_at(&self.file, buffer, offset)
    }

    fn block_count(&self) -> io::Result<u64> {
        Ok(self.file.metadata()?.len() / self.size.bytes() as u64)
    }

    fn is_empty(&self) -> io::Result<bool> {
        Ok(self.file.metadata()?.len() == 0)
    }

    fn lock(&self) -> io::Result<()> {
        lock_or_skip_where_unsupported(|| self.file.try_lock())
    }

    fn lock_shared(&self) -> io::Result<()> {
        lock_or_skip_where_unsupported(|| self.file.try_lock_shared())
    }

    fn set_block_count(&self, count: u64) -> io::Result<()> {
        self.file.set_len(self.size.offset(count)?)
    }

    fn sync(&self) -> io::Result<()> {
        if cfg!(windows) && self.read_only {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Windows flushes a file only through a handle with write access",
            ));
        }
        self.file.sync_all()
    }
}

#[cfg(unix)]
fn read_exact_at(file: &File, buffer: &mut [u8], offset: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buffer, offset)
}

#[cfg(unix)]
fn write_all_at(file: &File, buffer: &[u8], offset: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(file, buffer, offset)
}

#[cfg(windows)]
fn read_exact_at(file: &File, mut buffer: &mut [u8], mut offset: u64) -> io::Result<()> {
    while !buffer.is_empty() {
        match std::os::windows::fs::FileExt::seek_read(file, buffer, offset) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "read past the end of the file",
                ));
            }
            Ok(read) => {
                buffer = &mut buffer[read..];
                offset += read as u64;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(locked_elsewhere(error)),
        }
    }
    Ok(())
}

#[cfg(windows)]
const ERROR_LOCK_VIOLATION: i32 = 33;

#[cfg(windows)]
fn locked_elsewhere(error: io::Error) -> io::Error {
    if error.raw_os_error() == Some(ERROR_LOCK_VIOLATION) {
        io::Error::new(
            io::ErrorKind::WouldBlock,
            "the database file is already open",
        )
    } else {
        error
    }
}

#[cfg(windows)]
fn write_all_at(file: &File, mut buffer: &[u8], mut offset: u64) -> io::Result<()> {
    while !buffer.is_empty() {
        match std::os::windows::fs::FileExt::seek_write(file, buffer, offset) {
            Ok(0) => return Err(io::Error::from(io::ErrorKind::WriteZero)),
            Ok(written) => {
                buffer = &buffer[written..];
                offset += written as u64;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(locked_elsewhere(error)),
        }
    }
    Ok(())
}
