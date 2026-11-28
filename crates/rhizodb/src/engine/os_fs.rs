use std::collections::BTreeSet;
use std::fs;
use std::fs::File;
use std::fs::TryLockError;
use std::io;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::atomic::AtomicU64;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use super::block_file::BlockFile;
use super::block_file::BlockSize;
use super::block_file::FileSystem;
use super::session::SideFile;
use super::session::SideLock;
use super::session::SideWords;
use super::session::WORDS;
use rhizodb_shm::FileId;
use rhizodb_shm::Lock;
use rhizodb_shm::Mapping;

/// The files of one directory of the operating system's file system.
#[derive(Debug)]
pub(crate) struct OsFs {
    directory: PathBuf,
    _claim: Option<Claim>,
    read_only: bool,
}

static OPEN_FILES: Mutex<BTreeSet<FileId>> = Mutex::new(BTreeSet::new());

// ast-grep-ignore
// Locks of one process need not conflict, so a claim keeps it to one open of a file by identity.
#[derive(Debug)]
struct Claim(FileId);

impl Claim {
    fn take(path: PathBuf) -> io::Result<Self> {
        let id = rhizodb_shm::file_id(&File::open(path)?)?;
        let mut held = OPEN_FILES.lock().unwrap_or_else(PoisonError::into_inner);
        if !held.insert(id) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "the database file is already open read-only in this process",
            ));
        }
        Ok(Self(id))
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        OPEN_FILES
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.0);
    }
}

impl OsFs {
    pub(crate) fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
            _claim: None,
            read_only: false,
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
            _claim: Some(claim),
            read_only: true,
        })
    }

    /// The files of `directory`, for the one open of the database file `name` that this
    /// process may hold at a time, which takes part in a multi-process session.
    pub(crate) fn shared(directory: impl Into<PathBuf>, name: &str) -> io::Result<Self> {
        let directory = directory.into();
        let claim = Claim::take(directory.join(name))?;
        Ok(Self {
            directory,
            _claim: Some(claim),
            read_only: false,
        })
    }

    fn writable(&self) -> io::Result<()> {
        if self.read_only {
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

fn lock_patiently(try_lock: impl Fn() -> Result<(), TryLockError>) -> io::Result<()> {
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
            Err(TryLockError::Error(error)) => return Err(error),
        }
    }
}

fn lock_or_skip_where_unsupported(
    try_lock: impl Fn() -> Result<(), TryLockError>,
) -> io::Result<()> {
    match lock_patiently(try_lock) {
        Err(error) if error.kind() == io::ErrorKind::Unsupported => Ok(()),
        locked => locked,
    }
}

// ast-grep-ignore
// Past any block, so it never refuses a write on Windows; on Unix an fcntl lock converts atomically.
const SESSION_BYTE: u64 = 1 << 62;

// ast-grep-ignore
// Off Linux a phase 1 reader locks this byte, since a whole-file lock would hide sessions from its test.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
const READER_BYTE: u64 = SESSION_BYTE + 1;

fn try_range(file: &File, at: u64, kind: Lock) -> Result<(), TryLockError> {
    match rhizodb_shm::try_lock(file, at, 1, kind) {
        Ok(true) => Ok(()),
        Ok(false) => Err(TryLockError::WouldBlock),
        Err(error) => Err(TryLockError::Error(error)),
    }
}

fn held_by_others(file: &File, len: u64) -> io::Result<bool> {
    match rhizodb_shm::held_elsewhere(file, SESSION_BYTE, len, Lock::Exclusive) {
        Err(error) if error.kind() == io::ErrorKind::Unsupported => Ok(false),
        outcome => outcome,
    }
}

#[cfg(unix)]
fn another_session() -> io::Error {
    io::Error::new(
        io::ErrorKind::WouldBlock,
        "a session through another path to the database file holds it",
    )
}

// ast-grep-ignore
// Opened once per process, since closing any descriptor of it drops every classic fcntl lock.
#[derive(Debug)]
struct OsSide {
    file: File,
    path: PathBuf,
}

#[derive(Debug)]
struct OsWords(Mapping);

impl SideWords for OsWords {
    fn word(&self, index: usize) -> &AtomicU64 {
        &self.0.words()[index]
    }
}

impl OsSide {
    fn boxed(file: File, path: PathBuf) -> io::Result<Box<dyn SideFile>> {
        if !rhizodb_shm::is_local(&file)? {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "processes on other machines share no memory, so a session needs a local file \
                 system",
            ));
        }
        Ok(Box::new(Self { file, path }))
    }
}

impl SideFile for OsSide {
    fn lock(&self, at: SideLock, kind: Lock) -> io::Result<()> {
        rhizodb_shm::lock(&self.file, at.offset(), 1, kind)
    }

    fn try_lock(&self, at: SideLock, kind: Lock) -> io::Result<bool> {
        rhizodb_shm::try_lock(&self.file, at.offset(), 1, kind)
    }

    fn unlock(&self, at: SideLock) -> io::Result<()> {
        rhizodb_shm::unlock(&self.file, at.offset(), 1)
    }

    fn downgrade(&self, at: SideLock) -> io::Result<()> {
        rhizodb_shm::downgrade(&self.file, at.offset(), 1)
    }

    #[cfg(not(windows))]
    fn try_upgrade(&self, at: SideLock) -> io::Result<bool> {
        self.try_lock(at, Lock::Exclusive)
    }

    #[cfg(windows)]
    fn try_upgrade(&self, at: SideLock) -> io::Result<bool> {
        self.unlock(at)?;
        if self.try_lock(at, Lock::Exclusive)? {
            return Ok(true);
        }
        self.lock(at, Lock::Shared)?;
        Ok(false)
    }

    fn is_named(&self) -> io::Result<bool> {
        let named = match File::open(&self.path) {
            Ok(named) => named,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) if cfg!(windows) && error.kind() == io::ErrorKind::PermissionDenied => {
                return Ok(false);
            }
            Err(error) => return Err(error),
        };
        Ok(rhizodb_shm::file_id(&named)? == rhizodb_shm::file_id(&self.file)?)
    }

    fn unlink(&self) -> io::Result<()> {
        fs::remove_file(&self.path)
    }

    fn map(&self, grow: bool) -> io::Result<Box<dyn SideWords>> {
        if grow {
            rhizodb_shm::reserve(&self.file, (WORDS * 8) as u64)?;
        }
        Ok(Box::new(OsWords(Mapping::new(&self.file, WORDS)?)))
    }
}

impl FileSystem for OsFs {
    type File = OsFile;

    fn open_side(&self, name: &str) -> io::Result<Box<dyn SideFile>> {
        let path = self.directory.join(name);
        let file = fs::OpenOptions::new().read(true).write(true).open(&path)?;
        OsSide::boxed(file, path)
    }

    fn create_side(&self, name: &str) -> io::Result<Box<dyn SideFile>> {
        let path = self.directory.join(name);
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)?;
        #[cfg(unix)]
        if let Some(database) = name.strip_suffix("-shm")
            && let Ok(metadata) = fs::metadata(self.directory.join(database))
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = metadata.permissions().mode() & 0o666;
            let _ = file.set_permissions(fs::Permissions::from_mode(mode | ((mode & 0o444) >> 1)));
        }
        OsSide::boxed(file, path)
    }

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
            .write(!self.read_only)
            .open(self.directory.join(name))?;
        Ok(OsFile {
            file,
            size,
            read_only: self.read_only,
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

    fn remove_side(&self, name: &str) -> io::Result<()> {
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

impl OsFile {
    /// Locks the file for this handle alone, failing where the file system has no locks.
    pub(crate) fn lock_or_refuse_where_unsupported(&self) -> io::Result<()> {
        lock_patiently(|| self.file.try_lock())
    }

    /// Another handle to the file, as blocks of `size`, that shares this one's lock, so reads
    /// through it never wait on that lock.
    pub(crate) fn share(&self, size: BlockSize) -> io::Result<Self> {
        Ok(Self {
            file: self.file.try_clone()?,
            size,
            read_only: self.read_only,
        })
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

    fn held_elsewhere(&self) -> io::Result<bool> {
        held_by_others(&self.file, 1)
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn lock_shared(&self) -> io::Result<()> {
        lock_or_skip_where_unsupported(|| self.file.try_lock_shared())
    }

    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    fn lock_shared(&self) -> io::Result<()> {
        lock_or_skip_where_unsupported(|| try_range(&self.file, READER_BYTE, Lock::Shared))
    }

    // ast-grep-ignore
    // Linux keeps flock and range locks apart, and its flock unlocks before it turns shared.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn lock_session(&self, first: bool) -> io::Result<()> {
        if first && self.held_elsewhere()? {
            return Err(another_session());
        }
        lock_or_skip_where_unsupported(|| try_range(&self.file, SESSION_BYTE, Lock::Shared))?;
        self.lock_shared()
    }

    // ast-grep-ignore
    // BSD kernels keep flock and fcntl locks in one list, so a test runs only while no flock is held.
    #[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
    fn lock_session(&self, first: bool) -> io::Result<()> {
        self.file.unlock()?;
        lock_or_skip_where_unsupported(|| try_range(&self.file, SESSION_BYTE, Lock::Shared))?;
        if first && held_by_others(&self.file, 2)? {
            let _ = rhizodb_shm::unlock(&self.file, SESSION_BYTE, 1);
            return Err(another_session());
        }
        lock_or_skip_where_unsupported(|| self.file.try_lock_shared())
    }

    // ast-grep-ignore
    // A shared lock on Windows denies writes to what it covers, so this one lies past any block.
    #[cfg(windows)]
    fn lock_session(&self, _first: bool) -> io::Result<()> {
        lock_or_skip_where_unsupported(|| try_range(&self.file, SESSION_BYTE, Lock::Shared))?;
        let _ = self.file.unlock();
        Ok(())
    }

    fn unlock(&self) -> io::Result<()> {
        self.file.unlock()?;
        #[cfg(not(windows))]
        let _ = rhizodb_shm::unlock(&self.file, SESSION_BYTE, 1);
        Ok(())
    }

    fn identity(&self) -> io::Result<(u64, u128)> {
        let id = rhizodb_shm::file_id(&self.file)?;
        Ok((id.volume, id.file))
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

// ast-grep-ignore
// A phase 1 reader's whole-file shared lock refuses writes for the moment it checks for -shm.
#[cfg(windows)]
fn write_all_at(file: &File, buffer: &[u8], offset: u64) -> io::Result<()> {
    let start = Instant::now();
    let mut pause = Duration::from_micros(100);
    loop {
        match write_all_at_once(file, buffer, offset) {
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock
                    && start.elapsed() < WRITE_PATIENCE =>
            {
                WRITE_RETRIES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                thread::sleep(pause);
                pause = pause.saturating_mul(2).min(LONGEST_LOCK_PAUSE);
            }
            outcome => return outcome,
        }
    }
}

#[cfg(windows)]
static WRITE_RETRIES: AtomicU64 = AtomicU64::new(0);

/// How many times a write of this process to a database file has waited behind another process's
/// lock on the file.
#[cfg(windows)]
pub(crate) fn write_retries() -> u64 {
    WRITE_RETRIES.load(std::sync::atomic::Ordering::Relaxed)
}

#[cfg(windows)]
const WRITE_PATIENCE: Duration = Duration::from_secs(2);

#[cfg(windows)]
fn write_all_at_once(file: &File, mut buffer: &[u8], mut offset: u64) -> io::Result<()> {
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
