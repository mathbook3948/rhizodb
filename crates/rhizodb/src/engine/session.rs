use std::fmt;
use std::io;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::atomic::fence;
use std::thread;
use std::time::Duration;

use rhizodb_shm::Lock;

use super::block_file::BlockFile;
use super::block_file::FileSystem;
use super::error::EngineError;
use super::header::FORMAT_MINOR;
use super::header::Header;
use super::layout::BLOCK;
use crate::error::ErrorKind;
use crate::storage::StorageError;

/// The words of a side file that every process of a session maps.
pub(crate) const WORDS: usize = 4096;

/// How many processes a session holds at once.
pub(crate) const SLOTS: usize = 128;

const SLOT_WORDS: usize = 2;
const SLOT_BASE: usize = WORDS - SLOTS * SLOT_WORDS;
const HEADER_BASE: usize = 16;
const MAGIC: u64 = u64::from_le_bytes(*b"rhizoshm");
const LAYOUT: u64 = 1;

// ast-grep-ignore
// Far past any growth of the mapped words, since Windows refuses writes to bytes a lock covers.
const LOCKS_FROM: u64 = 1 << 40;

#[derive(Clone, Copy, Debug)]
enum Field {
    Magic,
    Layout,
    Minor,
    Build,
    DatabaseLow,
    DatabaseHigh,
    FileVolume,
    FileLow,
    FileHigh,
    HeaderSequence,
    LogEnd,
}

impl Field {
    const IDENTITY: [Field; 8] = [
        Field::Layout,
        Field::Minor,
        Field::Build,
        Field::DatabaseLow,
        Field::DatabaseHigh,
        Field::FileVolume,
        Field::FileLow,
        Field::FileHigh,
    ];

    fn index(self) -> usize {
        self as usize
    }
}

/// A lock of a session, each on a byte of the side file past its mapped words.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum SideLock {
    /// Held by an open while it elects the session's first process, and by the first process
    /// until it is ready, so that no two opens both take themselves for the first.
    Gate,
    /// Held shared by every process of the session, and exclusively by its first process while
    /// it gets ready.
    Session,
    /// Held exclusively by the process that writes the session's log and checkpoints.
    Write,
    /// Held exclusively by a process while its slot is in use.
    Slot(usize),
}

impl SideLock {
    /// The byte of the side file that the lock covers.
    pub(crate) fn offset(self) -> u64 {
        LOCKS_FROM
            + match self {
                SideLock::Gate => 0,
                SideLock::Session => 1,
                SideLock::Write => 2,
                SideLock::Slot(slot) => 64 + slot as u64,
            }
    }
}

/// The side file of a session as one process holds it: its locks, and the mapping of its words
/// once [`SideFile::map`] made one.
pub(crate) trait SideFile: Send + Sync + fmt::Debug {
    /// Takes `kind` of `at`, waiting while another process keeps it out.
    fn lock(&self, at: SideLock, kind: Lock) -> io::Result<()>;
    /// Takes `kind` of `at` without waiting, and returns whether it got it.
    fn try_lock(&self, at: SideLock, kind: Lock) -> io::Result<bool>;
    fn unlock(&self, at: SideLock) -> io::Result<()>;
    /// Turns the exclusive lock this process holds of `at` into a shared one with no moment
    /// at which it holds neither.
    fn downgrade(&self, at: SideLock) -> io::Result<()>;
    /// Turns the shared lock this process holds of `at` into an exclusive one when no other
    /// process holds it, and otherwise keeps the shared lock. Where the operating system has no
    /// atomic upgrade, the caller must hold [`SideLock::Gate`], so no other open takes `at` in
    /// between.
    fn try_upgrade(&self, at: SideLock) -> io::Result<bool>;
    /// Maps the side file's words, growing the file to hold them first when `grow` is true, and
    /// fails without mapping when the file is too short for them.
    fn map(&self, grow: bool) -> io::Result<Box<dyn SideWords>>;
    /// Whether the path this side file was opened by still names it, which it does not once the
    /// last process of a session removed it.
    fn is_named(&self) -> io::Result<bool>;
    /// Removes the side file's name, while this process holds the gate and the session lock
    /// exclusively.
    fn unlink(&self) -> io::Result<()>;
}

/// The mapped words of a side file.
pub(crate) trait SideWords: Send + Sync + fmt::Debug {
    /// The word at `index`, below [`WORDS`].
    fn word(&self, index: usize) -> &AtomicU64;
}

/// What a session's processes must agree on: the side file's layout, this build's engine and
/// graph formats, and the database.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Identity {
    /// The graph format and tokenizer of this build, hashed into one word.
    pub(crate) build: u64,
    pub(crate) database_id: u128,
    /// The database file's volume and file id, so two side files of one file reached through
    /// two paths never hold two sessions.
    pub(crate) file: (u64, u128),
}

impl Identity {
    fn words(self) -> [u64; 8] {
        [
            LAYOUT,
            u64::from(FORMAT_MINOR),
            self.build,
            self.database_id as u64,
            (self.database_id >> 64) as u64,
            self.file.0,
            self.file.1 as u64,
            (self.file.1 >> 64) as u64,
        ]
    }
}

/// The side file's name beside the database file `name`.
pub(crate) fn side_name(name: &str) -> String {
    format!("{name}-shm")
}

/// An open that elected itself or joined, before it read the database file.
#[derive(Debug)]
pub(crate) struct Elected {
    side: Box<dyn SideFile>,
    words: Box<dyn SideWords>,
    first: bool,
}

/// How an open that may take part in a session goes on.
#[derive(Debug)]
pub(crate) enum Joined {
    /// It is a process of a session, as its first process or not.
    Session(Elected),
    /// No session can hold the file here, so it opens alone as a phase 1 read-only open, and
    /// fails when `recheck` and a side file appeared once it holds its shared lock.
    Alone { recheck: bool },
}

fn already_open(message: &'static str) -> EngineError {
    io::Error::new(io::ErrorKind::WouldBlock, message).into()
}

fn being_removed(error: &io::Error) -> bool {
    cfg!(windows) && error.kind() == io::ErrorKind::PermissionDenied
}

const OPEN_TRIES: usize = 200;

// ast-grep-ignore
// The creator holds the file exclusively, so a phase 1 reader keeps it out or sees the side file.
fn open_or_create<F: FileSystem>(
    fs: &F,
    name: &str,
    file: &F::File,
) -> Result<Result<Box<dyn SideFile>, Joined>, EngineError> {
    for _ in 0..OPEN_TRIES {
        match fs.open_side(name) {
            Ok(side) => return Ok(Ok(side)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::Unsupported | io::ErrorKind::ReadOnlyFilesystem
                ) =>
            {
                return Ok(Err(Joined::Alone { recheck: false }));
            }
            Err(error) if being_removed(&error) => {
                thread::sleep(Duration::from_millis(5));
                continue;
            }
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                return Err(already_open(
                    "a multi-process session may hold the database file, and its -shm file \
                     cannot be opened for writing, since its permissions refuse this user",
                ));
            }
            Err(error) => return Err(error.into()),
        }
        match file.lock() {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                return Ok(Err(Joined::Alone { recheck: true }));
            }
            Err(error) => return Err(error.into()),
        }
        let created = fs.create_side(name);
        if matches!(&created, Err(error) if error.kind() == io::ErrorKind::Unsupported) {
            let _ = fs.remove(name);
        }
        file.unlock()?;
        match created {
            Ok(side) => return Ok(Ok(side)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) if being_removed(&error) => thread::sleep(Duration::from_millis(5)),
            Err(error) if error.kind() == io::ErrorKind::Unsupported => {
                return Ok(Err(Joined::Alone { recheck: false }));
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::PermissionDenied | io::ErrorKind::ReadOnlyFilesystem
                ) =>
            {
                return Ok(Err(Joined::Alone { recheck: true }));
            }
            Err(error) => return Err(error.into()),
        }
    }
    Err(already_open(
        "the -shm file of the database file kept being created and removed",
    ))
}

/// Opens the side file of the database file `name`, creating it under `file`'s exclusive lock
/// when there is none, and elects this open the session's first process or joins the session.
///
/// The first process holds `file` exclusively and the session's gate until
/// [`Elected::finish`], so no other open reads the file or joins while it gets ready.
pub(crate) fn join<F: FileSystem>(
    fs: &F,
    name: &str,
    file: &F::File,
) -> Result<Joined, EngineError> {
    for _ in 0..OPEN_TRIES {
        let side = match open_or_create(fs, &side_name(name), file)? {
            Ok(side) => side,
            Err(alone) => return Ok(alone),
        };
        side.lock(SideLock::Gate, Lock::Exclusive)?;
        if !side.is_named()? {
            side.unlock(SideLock::Gate)?;
            continue;
        }
        side.lock(SideLock::Session, Lock::Shared)?;
        let first = side.try_upgrade(SideLock::Session)?;
        if first {
            if let Err(error) = file.lock() {
                side.unlock(SideLock::Session)?;
                side.unlock(SideLock::Gate)?;
                return Err(error.into());
            }
        } else {
            side.unlock(SideLock::Gate)?;
        }
        let words = match side.map(first) {
            Ok(words) => words,
            Err(error) => {
                if first {
                    side.unlock(SideLock::Gate)?;
                }
                side.unlock(SideLock::Session)?;
                return Err(error.into());
            }
        };
        if first {
            words.word(Field::Magic.index()).store(0, Ordering::SeqCst);
        }
        return Ok(Joined::Session(Elected { side, words, first }));
    }
    Err(already_open(
        "the -shm file of the database file kept being removed while it was opened",
    ))
}

impl Elected {
    /// Takes the session's write lock for as long as this process is open, and fails when
    /// another process of the session writes.
    pub(crate) fn take_write_lock(&self) -> Result<(), EngineError> {
        if self.side.try_lock(SideLock::Write, Lock::Exclusive)? {
            return Ok(());
        }
        Err(already_open(
            "another process of the multi-process session writes the database",
        ))
    }

    /// Whether this open is the session's first process.
    pub(crate) fn is_first(&self) -> bool {
        self.first
    }

    /// The newest durable header that the session published, or `None` before any.
    pub(crate) fn published_header(&self) -> Result<Option<Header>, EngineError> {
        published_header(&*self.words)
    }

    /// Ends the election once the open read the database file: the first process resets the
    /// side file for `identity` and lets others join, a later one checks that it agrees with
    /// the session, and either takes `file`'s session lock and a slot for a process whose tree
    /// is of `generation`.
    pub(crate) fn finish(
        self,
        file: &impl BlockFile,
        identity: Identity,
        generation: u64,
    ) -> Result<Session, EngineError> {
        let Elected { side, words, first } = self;
        if first {
            reset(&*words, identity);
        } else {
            let magic = words.word(Field::Magic.index()).load(Ordering::SeqCst);
            let theirs =
                Field::IDENTITY.map(|field| words.word(field.index()).load(Ordering::SeqCst));
            if magic != MAGIC || theirs != identity.words() {
                side.unlock(SideLock::Session)?;
                return Err(already_open(
                    "a multi-process session of another build or layout holds the database file",
                ));
            }
        }
        let held = file
            .lock_session()
            .map_err(EngineError::from)
            .and_then(|()| claim_slot(&*side, &*words, generation));
        let slot = match held {
            Ok(slot) => slot,
            Err(error) => {
                if first {
                    let _ = side.unlock(SideLock::Gate);
                }
                let _ = side.unlock(SideLock::Session);
                return Err(error);
            }
        };
        Ok(Session {
            side,
            words,
            slot,
            getting_ready: AtomicBool::new(first),
            published: AtomicU64::new(u64::MAX),
        })
    }
}

fn published_header(words: &dyn SideWords) -> Result<Option<Header>, EngineError> {
    let sequence = words.word(Field::HeaderSequence.index());
    let mut block = vec![0; BLOCK];
    loop {
        let before = sequence.load(Ordering::Acquire);
        if before % 2 == 1 {
            std::hint::spin_loop();
            continue;
        }
        for (index, bytes) in block.as_chunks_mut::<8>().0.iter_mut().enumerate() {
            *bytes = words
                .word(HEADER_BASE + index)
                .load(Ordering::Relaxed)
                .to_le_bytes();
        }
        fence(Ordering::Acquire);
        if sequence.load(Ordering::Relaxed) != before {
            continue;
        }
        if before == 0 {
            return Ok(None);
        }
        return Header::decode(&block).map(Some);
    }
}

fn reset(words: &dyn SideWords, identity: Identity) {
    for index in 1..WORDS {
        words.word(index).store(0, Ordering::Relaxed);
    }
    for (field, value) in Field::IDENTITY.iter().zip(identity.words()) {
        words.word(field.index()).store(value, Ordering::Relaxed);
    }
    words
        .word(Field::Magic.index())
        .store(MAGIC, Ordering::SeqCst);
}

fn claim_slot(
    side: &dyn SideFile,
    words: &dyn SideWords,
    generation: u64,
) -> Result<usize, EngineError> {
    for slot in 0..SLOTS {
        if side.try_lock(SideLock::Slot(slot), Lock::Exclusive)? {
            words
                .word(SLOT_BASE + slot * SLOT_WORDS + 1)
                .store(0, Ordering::SeqCst);
            words
                .word(SLOT_BASE + slot * SLOT_WORDS)
                .store(generation, Ordering::SeqCst);
            return Ok(slot);
        }
    }
    Err(EngineError::Storage(StorageError::of_kind(
        ErrorKind::TooManyProcesses,
        io::Error::other("every slot of the multi-process session is in use"),
    )))
}

/// This process's part in a session: the side file, its words, and the slot it holds, which it
/// gives up when dropped.
#[derive(Debug)]
pub(crate) struct Session {
    side: Box<dyn SideFile>,
    words: Box<dyn SideWords>,
    slot: usize,
    getting_ready: AtomicBool,
    published: AtomicU64,
}

impl Session {
    /// Lets other opens join, once the first process published what they read. It does
    /// nothing in a process that joined.
    pub(crate) fn ready(&self) -> Result<(), EngineError> {
        if self.getting_ready.swap(false, Ordering::SeqCst) {
            self.side.downgrade(SideLock::Session)?;
            self.side.unlock(SideLock::Gate)?;
        }
        Ok(())
    }

    /// Whether this process is the last of its session, in which case it holds the session lock
    /// exclusively from here on, so that it closes as a process alone does. A process that is
    /// not the last holds its shared lock as before.
    pub(crate) fn leave_last(&self) -> Result<bool, EngineError> {
        if self.getting_ready.load(Ordering::SeqCst) {
            return Ok(true);
        }
        self.side.lock(SideLock::Gate, Lock::Exclusive)?;
        let last = self.side.try_upgrade(SideLock::Session);
        self.side.unlock(SideLock::Gate)?;
        Ok(last?)
    }

    /// Publishes `header`, which is durable, as the newest header of the session, and orders
    /// that store before every later load of a slot.
    pub(crate) fn publish_header(&self, header: &Header) {
        let block = header.encode();
        let sequence = self.words.word(Field::HeaderSequence.index());
        let before = sequence.fetch_add(1, Ordering::AcqRel);
        fence(Ordering::Release);
        for (index, bytes) in block.as_chunks::<8>().0.iter().enumerate() {
            self.words
                .word(HEADER_BASE + index)
                .store(u64::from_le_bytes(*bytes), Ordering::Relaxed);
        }
        sequence.store(before + 2, Ordering::Release);
        self.published.store(header.generation, Ordering::SeqCst);
        fence(Ordering::SeqCst);
    }

    /// The newest durable header that the session published, or `None` before any.
    pub(crate) fn published_header(&self) -> Result<Option<Header>, EngineError> {
        published_header(&*self.words)
    }

    /// Whether this process got the session ready and has not let others join yet.
    pub(crate) fn is_getting_ready(&self) -> bool {
        self.getting_ready.load(Ordering::SeqCst)
    }

    /// Publishes `lsn` as the last commit of the session's log, which is durable.
    pub(crate) fn publish_log_end(&self, lsn: u64) {
        self.words
            .word(Field::LogEnd.index())
            .store(lsn, Ordering::Release);
    }

    /// The last durable commit of the session's log.
    pub(crate) fn log_end(&self) -> u64 {
        self.words
            .word(Field::LogEnd.index())
            .load(Ordering::Acquire)
    }

    /// Records that every tree this process reaches is of `generation` or later, and orders
    /// that store before every later load of the published header.
    pub(crate) fn hold_generation(&self, generation: u64) {
        self.words
            .word(SLOT_BASE + self.slot * SLOT_WORDS)
            .store(generation, Ordering::SeqCst);
        fence(Ordering::SeqCst);
    }

    /// Records that this process pins a checkpoint whose log after `lsn` its snapshots need, or
    /// none, so the writer keeps that log as it keeps the log for a pin of its own.
    pub(crate) fn hold_pin(&self, lsn: Option<u64>) {
        self.words
            .word(SLOT_BASE + self.slot * SLOT_WORDS + 1)
            .store(lsn.map_or(0, |lsn| lsn + 1), Ordering::SeqCst);
    }

    /// The oldest LSN after which a snapshot pin of another live process needs the log.
    pub(crate) fn oldest_other_pin(&self) -> Option<u64> {
        (0..SLOTS)
            .filter(|slot| *slot != self.slot)
            .filter_map(|slot| {
                let pin = self
                    .words
                    .word(SLOT_BASE + slot * SLOT_WORDS + 1)
                    .load(Ordering::SeqCst);
                (pin != 0 && !self.slot_is_dead(slot).unwrap_or(true)).then(|| pin - 1)
            })
            .min()
    }

    // ast-grep-ignore
    // Cleared under the slot's own lock, so a process that claims it meanwhile keeps its words.
    fn clear_if_dead(&self, slot: usize) -> bool {
        if slot == self.slot
            || !self
                .side
                .try_lock(SideLock::Slot(slot), Lock::Exclusive)
                .unwrap_or(false)
        {
            return false;
        }
        for offset in 0..SLOT_WORDS {
            self.words
                .word(SLOT_BASE + slot * SLOT_WORDS + offset)
                .store(0, Ordering::SeqCst);
        }
        let _ = self.side.unlock(SideLock::Slot(slot));
        true
    }

    /// The oldest generation that a tree of another live process reaches, or that an open
    /// joining now may load from the header this process published last, clearing the slot of
    /// each process that is gone.
    pub(crate) fn oldest_other_generation(&self) -> u64 {
        fence(Ordering::SeqCst);
        let mut oldest = self.published.load(Ordering::SeqCst);
        for slot in (0..SLOTS).filter(|slot| *slot != self.slot) {
            let word = self.words.word(SLOT_BASE + slot * SLOT_WORDS);
            let generation = word.load(Ordering::SeqCst);
            if generation == 0 {
                continue;
            }
            if self.clear_if_dead(slot) {
                continue;
            }
            oldest = oldest.min(generation);
        }
        oldest
    }

    /// The slot this process holds.
    #[cfg(test)]
    pub(crate) fn slot(&self) -> usize {
        self.slot
    }

    /// The oldest checkpoint generation that a tree of the process in `slot` reaches, or zero
    /// for a free slot.
    #[cfg(test)]
    pub(crate) fn slot_generation(&self, slot: usize) -> u64 {
        self.words
            .word(SLOT_BASE + slot * SLOT_WORDS)
            .load(Ordering::SeqCst)
    }

    /// Whether the process that held `slot` is gone, which frees the slot for the caller to
    /// clear. A slot this process holds is never gone.
    pub(crate) fn slot_is_dead(&self, slot: usize) -> io::Result<bool> {
        if slot == self.slot {
            return Ok(false);
        }
        let free = self.side.try_lock(SideLock::Slot(slot), Lock::Exclusive)?;
        if free {
            self.side.unlock(SideLock::Slot(slot))?;
        }
        Ok(free)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.hold_pin(None);
        self.words
            .word(SLOT_BASE + self.slot * SLOT_WORDS)
            .store(0, Ordering::SeqCst);
        let _ = self.side.unlock(SideLock::Slot(self.slot));
        if self.side.lock(SideLock::Gate, Lock::Exclusive).is_ok() {
            if self.side.try_upgrade(SideLock::Session).unwrap_or(false) {
                let _ = self.side.unlink();
            }
            let _ = self.side.unlock(SideLock::Session);
            let _ = self.side.unlock(SideLock::Gate);
        }
    }
}

#[cfg(test)]
#[path = "session.test.rs"]
mod tests;
