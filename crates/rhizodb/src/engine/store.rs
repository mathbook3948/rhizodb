use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::hash::Hasher;
use std::io;
use std::ops::Bound;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;
use std::sync::TryLockError;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;

use super::blob::BlobRef;
use super::blob::RunRef;
use super::block_file::BlockFile;
use super::block_file::BlockSize;
use super::block_file::FileSystem;
use super::cache::Access;
use super::catalog::Root;
use super::changes::EpochId;
use super::check;
use super::check::CheckReport;
use super::checkpoint::Checkpointer;
use super::checkpoint::Finished;
use super::checkpoint::load_catalog;
use super::error::EngineError;
use super::extent::Blocks;
use super::extent::ExtentTable;
use super::header;
use super::header::FORMAT_MINOR;
use super::header::Header;
use super::header::PEER_LIMIT;
use super::header::Replication;
use super::header::Retained;
use super::header::WalIdentity;
use super::layout::BLOCK;
use super::layout::EXTENT_BLOCKS;
use super::layout::ExtentId;
use super::layout::TableId;
use super::memtable::Bytes;
use super::memtable::Memtable;
use super::memtable::Slot;
use super::node::CellValue;
use super::page::page_seed;
use super::pages::PageSource;
use super::pages::Pages;
use super::staged::Staged;
use super::staged::StagedOp;
use super::tree::Tree;
use super::view::Layers;
use super::view::View;
use super::view::Visit;
use super::view::prefix_end;
use super::wal::Detached;
use super::wal::LogIdentity;
use super::wal::Segment;
use super::wal::Wal;
use super::wal::retained_segments;
use super::wal::segment_files;
use super::wal::segment_name;
use super::wal_format::Body;
use super::wal_format::Step;
use super::wal_format::chain_after;
use super::wal_format::encode_record;
use super::wal_format::term_batch_record;
use crate::error::ErrorKind;
use crate::storage::Entry;
use crate::storage::Maintenance;
use crate::storage::Peek;
use crate::storage::ReadTransaction;
use crate::storage::Storage;
use crate::storage::StorageError;
use crate::storage::StorageStats;
use crate::storage::Table;
use crate::storage::WriteTransaction;

#[derive(Clone, Copy, Debug)]
enum Stage {
    Partial,
    Whole { clean: bool },
}

const STAGE_SHARE: u64 = 8;

fn parts(changes: &[(Bytes, Slot)], blocks: u64) -> Vec<&[(Bytes, Slot)]> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut held = 0;
    for (index, (key, slot)) in changes.iter().enumerate() {
        held += (key.len() + slot.as_ref().map_or(0, |value| value.len())) as u64;
        if held >= blocks.saturating_mul(BLOCK as u64) || (index + 1 - start) as u64 >= blocks {
            parts.push(&changes[start..=index]);
            start = index + 1;
            held = 0;
        }
    }
    if start < changes.len() {
        parts.push(&changes[start..]);
    }
    parts
}

/// Where checkpoints run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CheckpointMode {
    /// On a thread of the engine's own, while commits go on.
    Background,
    /// On the committing thread, so a test sees the same operations every run.
    #[cfg(any(test, fuzzing))]
    Inline,
}

/// How the engine is tuned. The defaults suit a database on disk.
#[derive(Clone, Copy, Debug)]
pub(crate) struct EngineOptions {
    /// The size a log segment grows to before the next record starts a new one.
    pub(crate) segment_target: u64,
    /// The memtable size that starts a checkpoint.
    pub(crate) memtable_limit: u64,
    /// The log size that starts a checkpoint.
    pub(crate) wal_limit: u64,
    pub(crate) checkpoint: CheckpointMode,
    /// The bytes of pages and values the block cache holds.
    pub(crate) cache_bytes: usize,
    /// The bytes of free space the file may hold beyond twice its live data and one extent
    /// before a checkpoint compacts it.
    pub(crate) reclaim_slack: u64,
    /// The fewest blocks a stage of a checkpoint frees before its header is written.
    pub(crate) stage_floor: u64,
    /// The most bytes of log segments that recovery no longer needs and that acknowledged peers
    /// keep.
    pub(crate) max_retained_bytes: u64,
    /// How long an export may pin a checkpoint before the engine releases it.
    pub(crate) pin_limit: Duration,
}

impl Default for EngineOptions {
    fn default() -> Self {
        Self {
            segment_target: 16 << 20,
            memtable_limit: 64 << 20,
            wal_limit: 256 << 20,
            checkpoint: CheckpointMode::Background,
            cache_bytes: 1 << 30,
            reclaim_slack: 0,
            stage_floor: 256,
            max_retained_bytes: 1 << 30,
            pin_limit: Duration::from_secs(3600),
        }
    }
}

pub(crate) fn block_size() -> Result<BlockSize, EngineError> {
    BlockSize::new(BLOCK).ok_or(EngineError::TooLarge)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

pub(crate) fn random_u64() -> u64 {
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u128(now_nanos());
    hasher.write_u32(std::process::id());
    hasher.finish()
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos())
}

fn hybrid_clock_after(last: u64) -> u64 {
    let wall = u64::try_from(now_nanos() / 1000).unwrap_or(u64::MAX);
    wall.max(last.saturating_add(1))
}

#[derive(Debug)]
struct Published {
    view: View,
    chain: u64,
    readers: BTreeMap<u64, usize>,
}

#[derive(Debug)]
struct Writer<F: FileSystem> {
    wal: Option<Wal<F>>,
    last_lsn: u64,
    last_hlc: u64,
    term: u64,
    chain: u64,
}

#[derive(Debug)]
struct Durable {
    header: Header,
    extents: ExtentTable,
    catalog: Vec<RunRef>,
    tree: Arc<Tree>,
}

#[derive(Debug)]
struct Job {
    frozen: Arc<Memtable>,
    lsn: u64,
    hlc: u64,
    chain: u64,
    first_segment: u64,
}

#[derive(Debug)]
enum Task<F: FileSystem> {
    Checkpoint(Job),
    Remove(Detached<F>),
}

#[derive(Clone, Copy, Debug)]
struct Identity {
    database_id: u128,
    replication: Replication,
}

#[derive(Debug)]
struct Shared<F: FileSystem> {
    options: EngineOptions,
    read_only: bool,
    fs: Arc<F>,
    name: String,
    epochs: Mutex<Vec<Epoch>>,
    accepting: AtomicBool,
    role: Mutex<Role>,
    pages: Arc<Pages<F::File>>,
    published: Mutex<Published>,
    writer: Mutex<Writer<F>>,
    durable: Mutex<Durable>,
    identity: Mutex<Identity>,
    droppable: Mutex<Option<u64>>,
    running: Mutex<bool>,
    finished: Condvar,
    failure: Mutex<Option<(bool, String)>>,
    trees: Mutex<Vec<Weak<Tree>>>,
    quarantine: Mutex<BTreeMap<u64, u64>>,
    freed: Mutex<BTreeMap<u64, Vec<(ExtentId, Blocks)>>>,
    poisoned: AtomicBool,
    keeping_free_space: AtomicBool,
    wal_bytes: AtomicU64,
    compacted_bytes: AtomicU64,
    needed: AtomicU64,
    log_first: AtomicU64,
    retention: Mutex<Retention>,
    compaction_due: AtomicBool,
    #[cfg(test)]
    clock_ahead: Mutex<Duration>,
}

#[derive(Debug, Default)]
struct Retention {
    peers: BTreeMap<Vec<u8>, u64>,
    pins: BTreeMap<u64, Pin>,
    next_pin: u64,
}

#[derive(Debug)]
struct Pin {
    tree: Arc<Tree>,
    lsn: u64,
    deadline: Option<Instant>,
}

impl Retention {
    fn oldest_peer(&self) -> Option<u64> {
        self.peers.values().min().copied()
    }

    fn oldest_pin(&self) -> Option<u64> {
        self.pins.values().map(|pin| pin.lsn).min()
    }

    fn expire(&mut self, now: Instant) -> bool {
        let before = self.pins.len();
        self.pins
            .retain(|_, pin| pin.deadline.is_none_or(|deadline| deadline > now));
        self.pins.len() != before
    }
}

/// RhizoDB's own storage engine over one database file and its write-ahead log.
#[derive(Debug)]
pub(crate) struct Engine<F: FileSystem + 'static> {
    shared: Arc<Shared<F>>,
    jobs: Option<mpsc::Sender<Task<F>>>,
    worker: Option<thread::JoinHandle<()>>,
}

pub(crate) fn read_header(file: &impl BlockFile) -> Result<(Header, Retained), EngineError> {
    if file.block_count()? < 2 {
        return Err(EngineError::corrupt(
            "the database file is shorter than its two header slots",
        ));
    }
    let mut slots = [vec![0; BLOCK], vec![0; BLOCK]];
    for (slot, block) in slots.iter_mut().enumerate() {
        file.read(slot as u64, block)?;
    }
    let header = header::choose(
        header::decode_slot(&slots[0], 0),
        header::decode_slot(&slots[1], 1),
    )?;
    let chosen = slots
        .get(usize::from(header.slot() == 1))
        .ok_or_else(|| EngineError::corrupt("a header slot is missing"))?;
    Ok((header, header::decode_retained(chosen)?))
}

fn write_new_database<F: BlockFile>(file: &F) -> Result<(), EngineError> {
    let database_id = (u128::from(random_u64()) << 64) | u128::from(random_u64());
    let header = Header {
        minor: FORMAT_MINOR,
        database_id,
        generation: 1,
        next_extent: ExtentId(1),
        clean: true,
        replication: Replication {
            incarnation: new_incarnation(),
            ..Replication::default()
        },
        ..Header::default()
    };
    file.set_block_count(0)?;
    file.write(0, &vec![0; BLOCK])?;
    file.write(header.slot(), &header.encode())?;
    file.sync()?;
    Ok(())
}

/// A random id for a new database file or a newly installed copy, which tells the writer that
/// began a term from a copy of it.
pub(crate) fn new_incarnation() -> u128 {
    (u128::from(random_u64()) << 64) | u128::from(random_u64())
}

/// The side file a multi-process session of a later build keeps beside the database file `name`.
pub(crate) fn session_file_name(name: &str) -> String {
    format!("{name}-shm")
}

pub(crate) fn staging_name(name: &str) -> String {
    format!(".{name}.creating")
}

fn ignore_missing(outcome: io::Result<()>) -> io::Result<()> {
    match outcome {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        outcome => outcome,
    }
}

const CREATION_WAIT: Duration = Duration::from_secs(10);

enum Existing<File> {
    Database(File),
    Missing,
}

fn existing_database<F: FileSystem>(fs: &F, name: &str) -> Result<Existing<F::File>, EngineError> {
    match fs.open(name, block_size()?) {
        Ok(file) if file.is_empty()? && segment_files(fs, name)?.is_empty() => {
            Ok(Existing::Missing)
        }
        Ok(file) => Ok(Existing::Database(file)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Existing::Missing),
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn open_or_create_staging<F: FileSystem>(
    fs: &F,
    staging: &str,
) -> Result<Option<F::File>, EngineError> {
    match fs.create(staging, block_size()?) {
        Ok(file) => Ok(Some(file)),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            match fs.open(staging, block_size()?) {
                Ok(file) => Ok(Some(file)),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(error) if is_being_removed(&error) => Ok(None),
                Err(error) => Err(error.into()),
            }
        }
        Err(error) if is_being_removed(&error) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn is_being_removed(error: &io::Error) -> bool {
    cfg!(windows) && error.kind() == io::ErrorKind::PermissionDenied
}

fn open_or_create_whole_or_not_at_all<F: FileSystem>(
    fs: &F,
    name: &str,
) -> Result<F::File, EngineError> {
    let staging = staging_name(name);
    let deadline = Instant::now() + CREATION_WAIT;
    loop {
        match try_open_or_create(fs, name, &staging) {
            Ok(Some(file)) => return Ok(file),
            Ok(None) => {}
            Err(EngineError::Io(error)) if is_being_removed(&error) => {}
            Err(error) => return Err(error),
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "another open kept creating the database file",
            )
            .into());
        }
        thread::sleep(Duration::from_millis(2));
    }
}

fn try_open_or_create<F: FileSystem>(
    fs: &F,
    name: &str,
    staging: &str,
) -> Result<Option<F::File>, EngineError> {
    if let Existing::Database(file) = existing_database(fs, name)? {
        file.lock()?;
        ignore_missing(fs.remove(staging))?;
        return Ok(Some(file));
    }
    let Some(file) = open_or_create_staging(fs, staging)? else {
        return Ok(None);
    };
    match file.lock() {
        Ok(()) => {
            if let Existing::Database(_) = existing_database(fs, name)? {
                drop(file);
                ignore_missing(fs.remove(staging))?;
                return Ok(None);
            }
            write_new_database(&file)?;
            fs.rename(staging, name)?;
            fs.sync_names()?;
            Ok(Some(file))
        }
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn find_live_blocks(
    header: &Header,
    extents: &mut ExtentTable,
    counts: &BTreeMap<ExtentId, u32>,
    catalog: &[RunRef],
    tree: &Tree,
) -> Result<(), EngineError> {
    let mut reached = check::reachable(header, extents, catalog, tree)?;
    for (id, entry) in &mut extents.entries {
        let used = reached.remove(id).unwrap_or_default();
        if Some(&used.count()) != counts.get(id) {
            return Err(EngineError::corrupt(format!(
                "extent {} counts other live blocks than its trees reach",
                id.0
            )));
        }
        entry.used = used;
    }
    if !reached.is_empty() {
        return Err(EngineError::corrupt(
            "a tree reaches an extent that is not in the table",
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug)]
struct Replayed {
    lsn: u64,
    hlc: u64,
    chain: u64,
}

/// A database file read up to its tree: the header, the catalog, and the extent table.
#[derive(Debug)]
pub(crate) struct Loaded<B: BlockFile> {
    pages: Arc<Pages<B>>,
    header: Header,
    retained: Retained,
    tree: Arc<Tree>,
    extents: ExtentTable,
    catalog: Vec<RunRef>,
}

impl<B: BlockFile + 'static> Loaded<B> {
    /// Checks every invariant of the loaded file, as [`Engine::check`] does.
    pub(crate) fn check(&self) -> Result<CheckReport, EngineError> {
        check::check(&self.header, &self.extents, &self.catalog, &self.tree)
    }

    pub(crate) fn header(&self) -> &Header {
        &self.header
    }

    /// The newest term the loaded tree's replication history records.
    pub(crate) fn newest_term(&self) -> Result<u64, EngineError> {
        let layers = Arc::new(Layers {
            active: Arc::new(Memtable::default()),
            frozen: None,
            tree: Arc::clone(&self.tree),
        });
        Ok(replica::load_epochs(&layers)?
            .last()
            .map_or(0, |epoch| epoch.term))
    }

    /// Writes `header` over the loaded file's current header slot, with no log kept for peers,
    /// and syncs it.
    pub(crate) fn rewrite_header(&mut self, header: Header) -> Result<(), EngineError> {
        if header.generation != self.header.generation {
            return Err(EngineError::corrupt(
                "a rewritten header changed its generation",
            ));
        }
        self.pages.write_header(&header, &Retained::default())?;
        self.header = header;
        self.retained = Retained::default();
        Ok(())
    }
}

/// Reads the header, catalog, and extent table of `file`.
pub(crate) fn load<B: BlockFile + 'static>(
    file: B,
    options: &EngineOptions,
) -> Result<Loaded<B>, EngineError> {
    let (header, retained) = read_header(&file)?;
    let pages = Arc::new(Pages::new(
        file,
        page_seed(header.database_id),
        options.cache_bytes,
    ));
    let source: Arc<dyn PageSource> = Arc::clone(&pages) as Arc<dyn PageSource>;
    let (tree, extents, catalog) = if header.catalog.is_null() {
        if header.open.iter().any(|open| !open.id.is_none()) || header.checkpoint_lsn != 0 {
            return Err(EngineError::corrupt(
                "a header without a catalog records extents",
            ));
        }
        let extents = ExtentTable {
            next_extent: header.next_extent,
            ..ExtentTable::default()
        };
        let tree = Tree::new(header.generation, BTreeMap::new(), Arc::clone(&source));
        (Arc::new(tree), extents, Vec::new())
    } else {
        for open in header.open.iter().filter(|open| !open.id.is_none()) {
            pages.place_extent(open.id, open.slot);
        }
        let first = RunRef {
            at: header.catalog,
            blocks: 1,
            lsn: header.catalog_lsn,
        };
        let (catalog, runs) =
            load_catalog(&source, first, &|id, slot| pages.place_extent(id, slot))?;
        if catalog.extents.next_extent != header.next_extent {
            return Err(EngineError::corrupt(
                "the header and catalog disagree on extents",
            ));
        }
        catalog.extents.check_open(&header.open)?;
        for (id, entry) in &catalog.extents.entries {
            pages.place_extent(*id, entry.slot);
        }
        let tree = Tree::new(header.generation, catalog.roots, Arc::clone(&source));
        let mut extents = catalog.extents;
        extents.fit_file(pages.file_bytes()? / BLOCK as u64)?;
        if let Some(counts) = catalog.live_counts {
            find_live_blocks(&header, &mut extents, &counts, &runs, &tree)?;
        }
        (Arc::new(tree), extents, runs)
    };
    Ok(Loaded {
        pages,
        header,
        retained,
        tree,
        extents,
        catalog,
    })
}

fn replay_into(
    layers: &Arc<Layers>,
    replayed: &mut Replayed,
    record: super::wal_format::Record,
) -> Result<(), EngineError> {
    if record.prev.is_some_and(|prev| prev != replayed.chain) {
        return Err(EngineError::corrupt(format!(
            "the log record at LSN {} does not follow the record before it",
            record.lsn
        )));
    }
    let chain = chain_after(replayed.chain, &encode_record(&record)?);
    let mut staged = Staged::new(View::new(replayed.lsn, Arc::clone(layers)));
    stage_record(&mut staged, record.lsn, record.term, record.body)?;
    layers
        .active
        .apply(record.lsn, staged.into_changes(), record.lsn);
    *replayed = Replayed {
        lsn: record.lsn,
        hlc: replayed.hlc.max(record.hlc),
        chain,
    };
    Ok(())
}

const RETIRED_POLL: Duration = Duration::from_millis(50);

fn remove_segments_before<F: FileSystem>(
    fs: &F,
    name: &str,
    sequence: u64,
) -> Result<(), EngineError> {
    for earlier in segment_files(fs, name)?
        .into_iter()
        .filter(|earlier| *earlier < sequence)
    {
        fs.remove(&segment_name(name, earlier))?;
    }
    Ok(())
}

#[expect(
    clippy::too_many_arguments,
    reason = "a new log takes the header, the log kept for peers, and where it starts at once"
)]
fn start_log<F: FileSystem>(
    fs: &Arc<F>,
    name: &str,
    pages: &Pages<F::File>,
    header: &Header,
    retained: &Retained,
    last_lsn: u64,
    hlc: u64,
    segment_target: u64,
) -> Result<(Wal<F>, Header), EngineError> {
    let identity = LogIdentity {
        database_id: header.database_id,
        shard_id: header.shard_id,
        salt: random_u64(),
    };
    let kept = if retained.from == 0 {
        Vec::new()
    } else {
        let until = segment_files(&**fs, name)?
            .last()
            .map_or(0, |sequence| sequence + 1);
        retained_segments(&**fs, name, identity, retained.from, until, last_lsn + 1)?
    };
    let sequence = kept
        .last()
        .map_or(header.wal.first_segment, Segment::sequence)
        .max(header.wal.first_segment)
        + 1;
    for later in segment_files(&**fs, name)?
        .into_iter()
        .filter(|later| *later >= sequence)
    {
        fs.remove(&segment_name(name, later))?;
    }
    let wal = Wal::create(
        Arc::clone(fs),
        name,
        identity,
        sequence,
        last_lsn + 1,
        hlc,
        segment_target,
        kept,
    )?;
    let header = Header {
        generation: header.generation + 1,
        wal: WalIdentity {
            first_segment: sequence,
            salt: identity.salt,
        },
        clean: false,
        replication: Replication {
            incarnation: match header.replication.incarnation {
                0 => new_incarnation(),
                incarnation => incarnation,
            },
            ..header.replication
        },
        ..*header
    };
    let kept = Retained {
        from: if wal.first_sequence() < sequence {
            wal.first_sequence()
        } else {
            0
        },
        peers: retained.peers.clone(),
    };
    pages.write_header(&header, &kept)?;
    remove_segments_before(&**fs, name, wal.first_sequence())?;
    Ok((wal, header))
}

// ast-grep-ignore
// Windows cannot sync a file opened without write access, so recovery there fails instead.
fn make_replayed_log_durable<F: FileSystem>(fs: &F, wal: &Wal<F>) -> Result<(), EngineError> {
    let synced = wal.sync_segments().and_then(|()| Ok(fs.sync_names()?));
    match synced {
        Err(EngineError::Io(error)) if error.kind() == io::ErrorKind::Unsupported => {
            Err(EngineError::ReadOnly(
                "make the log of an unclean shutdown durable on this platform, so open it once \
                 for writing",
            ))
        }
        synced => synced,
    }
}

fn make_recovery_durable<F: FileSystem + 'static>(
    shared: &Shared<F>,
    fs: &Arc<F>,
    name: &str,
) -> Result<(), EngineError> {
    let mut writer = lock(&shared.writer);
    shared.keeping_free_space.store(true, Ordering::Release);
    for _ in 0..2 {
        if lock(&shared.durable).header.clean {
            break;
        }
        if let Some(job) = shared.freeze(&mut writer) {
            shared.run_checkpoint(job, true);
        }
        if shared.is_poisoned() {
            return Err(shared.refusal());
        }
    }
    shared.keeping_free_space.store(false, Ordering::Release);
    let mut durable = lock(&shared.durable);
    if !durable.header.clean {
        return Err(EngineError::Poisoned);
    }
    lock(&shared.droppable).take();
    let recovered_from = writer.wal.take().map_or(0, |wal| wal.first_sequence());
    let hlc = hybrid_clock_after(writer.last_hlc);
    let (wal, header) = start_log(
        fs,
        name,
        &shared.pages,
        &durable.header,
        &Retained {
            from: recovered_from,
            peers: shared.retained_area(&durable.header).peers,
        },
        writer.last_lsn,
        hlc,
        shared.options.segment_target,
    )?;
    shared
        .needed
        .store(header.wal.first_segment, Ordering::Relaxed);
    *lock(&shared.identity) = Identity {
        database_id: header.database_id,
        replication: header.replication,
    };
    durable.header = header;
    drop(durable);
    writer.last_hlc = hlc;
    shared.wal_bytes.store(wal.size(), Ordering::Relaxed);
    shared
        .log_first
        .store(wal.first_sequence(), Ordering::Relaxed);
    writer.wal = Some(wal);
    if shared.holds_too_much_free_space() {
        shared.compact_unless_held();
    }
    if shared.is_poisoned() {
        return Err(shared.refusal());
    }
    Ok(())
}

fn checkpoint_worker<F: FileSystem + 'static>(
    shared: &Shared<F>,
    receiver: &mpsc::Receiver<Task<F>>,
) {
    let mut retired: Vec<Arc<Memtable>> = Vec::new();
    loop {
        let task = if retired.is_empty() && !shared.compaction_due.load(Ordering::Acquire) {
            receiver.recv().ok()
        } else {
            match receiver.recv_timeout(RETIRED_POLL) {
                Ok(task) => Some(task),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    retired.retain(|memtable| Arc::strong_count(memtable) > 1);
                    shared.compact_when_due();
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => None,
            }
        };
        let Some(task) = task else {
            return;
        };
        match task {
            Task::Checkpoint(job) => {
                retired.push(Arc::clone(&job.frozen));
                shared.run_checkpoint(job, false);
            }
            Task::Remove(detached) => detached.remove(),
        }
        retired.retain(|memtable| Arc::strong_count(memtable) > 1);
    }
}

impl<F: FileSystem + 'static> Engine<F> {
    /// Opens the database file `name` on `fs`, creating it when there is none, and replays the
    /// log of an unclean shutdown before it returns.
    pub(crate) fn open(
        fs: Arc<F>,
        name: &str,
        options: EngineOptions,
    ) -> Result<Self, EngineError> {
        let file = open_or_create_whole_or_not_at_all(&*fs, name)?;
        Self::open_loaded(fs, name, load(file, &options)?, options)
    }

    /// Opens the existing database file `name` on `fs` under a shared lock, which any number of
    /// read-only opens hold at once and which keeps out every open that writes. It writes no
    /// file: the log of an unclean shutdown is replayed into memory alone, and every write,
    /// checkpoint, and compaction is refused.
    pub(crate) fn open_read_only(
        fs: Arc<F>,
        name: &str,
        options: EngineOptions,
    ) -> Result<Self, EngineError> {
        let file = fs.open(name, block_size()?)?;
        file.lock_shared()?;
        if fs.names()?.contains(&session_file_name(name)) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "a multi-process session may hold the database file, since its -shm file exists, \
                 and a read-only open of this build takes no part in one",
            )
            .into());
        }
        Self::start(fs, name, load(file, &options)?, options, true)
    }

    /// Opens the database `loaded` read from the locked file named `name`, as [`Engine::open`]
    /// does once it holds the file.
    pub(crate) fn open_loaded(
        fs: Arc<F>,
        name: &str,
        loaded: Loaded<F::File>,
        options: EngineOptions,
    ) -> Result<Self, EngineError> {
        Self::start(fs, name, loaded, options, false)
    }

    fn start(
        fs: Arc<F>,
        name: &str,
        loaded: Loaded<F::File>,
        options: EngineOptions,
        read_only: bool,
    ) -> Result<Self, EngineError> {
        let Loaded {
            pages,
            header,
            retained,
            tree,
            extents,
            catalog,
        } = loaded;
        let layers = Arc::new(Layers {
            active: Arc::new(Memtable::default()),
            frozen: None,
            tree: Arc::clone(&tree),
        });
        let mut replayed = Replayed {
            lsn: header.checkpoint_lsn,
            hlc: header.checkpoint_hlc,
            chain: header.replication.checkpoint_chain,
        };
        let recovered = !header.clean;
        let (wal, header) = if read_only {
            let wal = if header.clean {
                None
            } else {
                let wal = Wal::replay(
                    Arc::clone(&fs),
                    name,
                    &header,
                    options.segment_target,
                    |record| replay_into(&layers, &mut replayed, record),
                )?;
                make_replayed_log_durable(&*fs, &wal)?;
                Some(wal)
            };
            (wal, header)
        } else if header.clean {
            replayed.hlc = hybrid_clock_after(replayed.hlc);
            let (wal, header) = start_log(
                &fs,
                name,
                &pages,
                &header,
                &retained,
                replayed.lsn,
                replayed.hlc,
                options.segment_target,
            )?;
            (Some(wal), header)
        } else {
            let wal = Wal::recover(
                Arc::clone(&fs),
                name,
                &header,
                retained.from,
                options.segment_target,
                |record| replay_into(&layers, &mut replayed, record),
            )?;
            (Some(wal), header)
        };
        let Replayed {
            lsn: last_lsn,
            hlc: last_hlc,
            chain,
        } = replayed;
        let needed = header.wal.first_segment;
        let log_first = wal.as_ref().map_or(needed, Wal::first_sequence);
        let wal_bytes = wal.as_ref().map_or(0, Wal::size);
        let wal = wal.filter(|_| !read_only);
        let epochs = replica::load_epochs(&layers)?;
        let term = epochs.last().map_or(0, |epoch| epoch.term);
        let role = replica::role_at_open(&header, &epochs);
        let shared = Arc::new(Shared {
            options,
            read_only,
            fs: Arc::clone(&fs),
            name: name.to_owned(),
            epochs: Mutex::new(epochs),
            accepting: AtomicBool::new(role == Role::Writer && !read_only),
            role: Mutex::new(role),
            pages,
            published: Mutex::new(Published {
                view: View::new(last_lsn, layers),
                chain,
                readers: BTreeMap::new(),
            }),
            wal_bytes: AtomicU64::new(wal_bytes),
            writer: Mutex::new(Writer {
                wal,
                last_lsn,
                last_hlc,
                term,
                chain,
            }),
            trees: Mutex::new(vec![Arc::downgrade(&tree)]),
            identity: Mutex::new(Identity {
                database_id: header.database_id,
                replication: header.replication,
            }),
            durable: Mutex::new(Durable {
                header,
                extents,
                catalog,
                tree,
            }),
            droppable: Mutex::new(None),
            running: Mutex::new(false),
            finished: Condvar::new(),
            failure: Mutex::new(None),
            quarantine: Mutex::new(BTreeMap::new()),
            freed: Mutex::new(BTreeMap::new()),
            poisoned: AtomicBool::new(false),
            keeping_free_space: AtomicBool::new(false),
            compacted_bytes: AtomicU64::new(0),
            needed: AtomicU64::new(needed),
            log_first: AtomicU64::new(log_first),
            retention: Mutex::new(Retention {
                peers: retained.peers.into_iter().collect(),
                ..Retention::default()
            }),
            compaction_due: AtomicBool::new(false),
            #[cfg(test)]
            clock_ahead: Mutex::new(Duration::ZERO),
        });
        if recovered && !read_only {
            make_recovery_durable(&shared, &fs, name)?;
        }
        let (jobs, worker) = match options.checkpoint {
            _ if read_only => (None, None),
            #[cfg(any(test, fuzzing))]
            CheckpointMode::Inline => (None, None),
            CheckpointMode::Background => {
                let (sender, receiver) = mpsc::channel::<Task<F>>();
                let worker_shared = Arc::clone(&shared);
                let worker = thread::Builder::new()
                    .name("rhizodb-checkpoint".to_owned())
                    .spawn(move || checkpoint_worker(&worker_shared, &receiver))?;
                (Some(sender), Some(worker))
            }
        };
        Ok(Self {
            shared,
            jobs,
            worker,
        })
    }

    fn register(&self) -> View {
        let mut published = lock(&self.shared.published);
        let view = published.view.clone();
        *published.readers.entry(view.snapshot).or_default() += 1;
        view
    }

    /// How many entries scans have copied out of the memtables the published view reads.
    #[cfg(test)]
    pub(crate) fn memtable_copied(&self) -> u64 {
        let layers = Arc::clone(&lock(&self.shared.published).view.layers);
        std::iter::once(&layers.active)
            .chain(&layers.frozen)
            .map(|memtable| memtable.copied.load(Ordering::Relaxed))
            .sum()
    }

    /// How many point lookups have read the tree the published view reads.
    #[cfg(test)]
    pub(crate) fn tree_lookups(&self) -> u64 {
        let layers = Arc::clone(&lock(&self.shared.published).view.layers);
        layers.tree.lookups.load(Ordering::Relaxed)
    }

    fn deregister(&self, snapshot: u64) {
        let mut published = lock(&self.shared.published);
        if let Some(count) = published.readers.get_mut(&snapshot) {
            *count -= 1;
            if *count == 0 {
                published.readers.remove(&snapshot);
            }
        }
    }

    fn commit(&self, writer: &mut Writer<F>, staged: Staged) -> Result<(), EngineError> {
        let shared = &*self.shared;
        if shared.is_poisoned() {
            return Err(shared.refusal());
        }
        if !shared.accepting.load(Ordering::Acquire) {
            return Err(replica::not_writer());
        }
        if staged.ops.is_empty() {
            return Ok(());
        }
        self.drop_segments_aside(writer);
        let lsn = writer.last_lsn + 1;
        let hlc = hybrid_clock_after(writer.last_hlc);
        let record = term_batch_record(
            lsn,
            writer.term,
            hlc,
            u128::from(lsn),
            writer.chain,
            staged.ops.iter().map(StagedOp::borrowed),
        )?;
        self.append(writer, record, lsn, hlc)?;
        self.publish(writer, lsn, staged);
        Ok(())
    }

    fn append(
        &self,
        writer: &mut Writer<F>,
        record: Vec<u8>,
        lsn: u64,
        hlc: u64,
    ) -> Result<(), EngineError> {
        let chain = chain_after(writer.chain, &record);
        let wal = writer.wal.as_mut().ok_or(EngineError::Poisoned)?;
        if let Err(error) = wal.append(record, hlc) {
            self.shared.poison_with(&error);
            return Err(error);
        }
        writer.chain = chain;
        writer.last_lsn = lsn;
        writer.last_hlc = writer.last_hlc.max(hlc);
        Ok(())
    }

    fn publish(&self, writer: &mut Writer<F>, lsn: u64, staged: Staged) {
        let shared = &*self.shared;
        let (layers, horizon) = {
            let published = lock(&shared.published);
            let snapshot = published.view.snapshot;
            let oldest = published.readers.keys().next().copied().unwrap_or(snapshot);
            (Arc::clone(&published.view.layers), oldest.min(snapshot))
        };
        let graves = staged.graves(&layers);
        let changes: Vec<(TableId, Bytes, Slot)> = staged.into_changes().collect();
        layers.active.apply(lsn, changes, horizon);
        layers.active.bury(lsn, graves);
        {
            let mut published = lock(&shared.published);
            published.view.snapshot = lsn;
            published.chain = writer.chain;
        }
        let (wal_bytes, wal_size) = writer.wal.as_ref().map_or((0, 0), |wal| {
            (
                wal.size(),
                wal.size_from(shared.needed.load(Ordering::Relaxed)),
            )
        });
        shared.wal_bytes.store(wal_bytes, Ordering::Relaxed);
        let memtable = layers.active.bytes();
        drop(layers);
        if memtable > shared.options.memtable_limit || wal_size > shared.options.wal_limit {
            self.start_checkpoint(writer);
            if memtable > shared.options.memtable_limit.saturating_mul(2) {
                shared.wait_for_checkpoint();
            }
        }
    }

    fn drop_segments_aside(&self, writer: &mut Writer<F>) {
        let Some(sequence) = lock(&self.shared.droppable).take() else {
            return;
        };
        let Some(wal) = writer.wal.as_mut() else {
            return;
        };
        let detached = wal.detach_before(self.shared.retained_from(wal, sequence));
        self.shared.wal_bytes.store(wal.size(), Ordering::Relaxed);
        self.shared
            .log_first
            .store(wal.first_sequence(), Ordering::Relaxed);
        if detached.is_empty() {
            return;
        }
        let unsent = match &self.jobs {
            Some(jobs) => match jobs.send(Task::Remove(detached)) {
                Ok(()) => None,
                Err(mpsc::SendError(Task::Remove(detached))) => Some(detached),
                Err(mpsc::SendError(Task::Checkpoint(_))) => None,
            },
            None => Some(detached),
        };
        if let Some(detached) = unsent {
            detached.remove();
        }
    }

    fn start_checkpoint(&self, writer: &mut Writer<F>) {
        let Some(job) = self.shared.freeze(writer) else {
            return;
        };
        match &self.jobs {
            Some(jobs) => {
                if let Err(mpsc::SendError(Task::Checkpoint(job))) =
                    jobs.send(Task::Checkpoint(job))
                {
                    self.shared.run_checkpoint(job, false);
                }
            }
            None => {
                self.shared.run_checkpoint(job, false);
                self.shared.drop_segments(writer);
            }
        }
    }

    /// Writes every commit so far into the database file and keeps the log, which the next
    /// checkpoint releases as any checkpoint does.
    pub(crate) fn checkpoint_keeping_log(&self) -> Result<(), EngineError> {
        self.writable("checkpoint")?;
        let mut writer = lock(&self.shared.writer);
        self.shared.wait_for_checkpoint();
        if self.shared.is_poisoned() {
            return Err(self.shared.refusal());
        }
        if let Some(job) = self.shared.freeze(&mut writer) {
            self.shared.run_checkpoint(job, false);
        }
        lock(&self.shared.droppable).take();
        if self.shared.is_poisoned() {
            return Err(self.shared.refusal());
        }
        Ok(())
    }

    #[cfg(any(test, fuzzing))]
    /// Writes every commit so far into the database file and drops the log segments it no
    /// longer needs.
    pub(crate) fn checkpoint(&self) -> Result<(), EngineError> {
        self.writable("checkpoint")?;
        let mut writer = lock(&self.shared.writer);
        self.shared.wait_for_checkpoint();
        if self.shared.is_poisoned() {
            return Err(self.shared.refusal());
        }
        if let Some(job) = self.shared.freeze(&mut writer) {
            self.shared.run_checkpoint(job, false);
        }
        self.shared.drop_segments(&mut writer);
        if self.shared.is_poisoned() {
            return Err(self.shared.refusal());
        }
        Ok(())
    }

    /// Writes every commit into the database file, removes the log, and closes the database,
    /// failing unless it closed cleanly.
    pub(crate) fn close(mut self) -> Result<(), EngineError> {
        self.stop_checkpoints();
        self.shared.close();
        if self.shared.read_only {
            return Ok(());
        }
        if self.shared.is_poisoned() {
            return Err(self.shared.refusal());
        }
        if !lock(&self.shared.durable).header.clean {
            return Err(EngineError::Poisoned);
        }
        Ok(())
    }

    pub(crate) fn is_read_only(&self) -> bool {
        self.shared.read_only
    }

    /// Fails with [`EngineError::ReadOnly`], naming `what` was asked, when the database was
    /// opened read-only.
    pub(crate) fn writable(&self, what: &'static str) -> Result<(), EngineError> {
        if self.is_read_only() {
            return Err(EngineError::ReadOnly(what));
        }
        Ok(())
    }

    fn stop_checkpoints(&mut self) {
        drop(self.jobs.take());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }

    /// Checks every invariant of the database file as the last durable checkpoint left it.
    pub(crate) fn check(&self) -> Result<CheckReport, EngineError> {
        let (header, extents, catalog, tree) = {
            let durable = lock(&self.shared.durable);
            (
                durable.header,
                durable.extents.clone(),
                durable.catalog.clone(),
                Arc::clone(&durable.tree),
            )
        };
        check::check(&header, &extents, &catalog, &tree)
    }

    /// Writes every commit into the database file, copies every table into as few extents as
    /// they need, and cuts the file after them.
    pub(crate) fn compact(&self) -> Result<(), EngineError> {
        self.writable("compact the file")?;
        let mut writer = lock(&self.shared.writer);
        self.shared.wait_for_checkpoint();
        if self.shared.is_poisoned() {
            return Err(self.shared.refusal());
        }
        if let Some(job) = self.shared.freeze(&mut writer) {
            self.shared.run_checkpoint(job, false);
        }
        self.shared.drop_segments(&mut writer);
        if self.shared.is_poisoned() {
            return Err(self.shared.refusal());
        }
        if let Err(error) = self.shared.compact() {
            self.shared.poison_with(&error);
            return Err(error);
        }
        Ok(())
    }

    /// Keeps the log after `lsn` for `peer`, in place of what it kept for that peer before,
    /// until the retained log passes the cap.
    pub(crate) fn acknowledge(&self, peer: &[u8], lsn: u64) -> Result<(), EngineError> {
        self.writable("keep the log for a peer")?;
        {
            let mut retention = lock(&self.shared.retention);
            if !retention.peers.contains_key(peer) && retention.peers.len() >= PEER_LIMIT {
                return Err(EngineError::Storage(StorageError::of_kind(
                    ErrorKind::TooManyPeers,
                    io::Error::other(format!(
                        "the database keeps the log for at most {PEER_LIMIT} peers"
                    )),
                )));
            }
            retention.peers.insert(peer.to_vec(), lsn);
        }
        self.shared.reconsider_retention();
        Ok(())
    }

    /// Stops keeping the log for `peer`, and returns whether it kept any.
    pub(crate) fn forget(&self, peer: &[u8]) -> bool {
        let forgotten = lock(&self.shared.retention).peers.remove(peer).is_some();
        self.shared.reconsider_retention();
        forgotten
    }

    /// Pins the last durable checkpoint: no block it reaches is written or cut off, and the log
    /// after it is kept as an acknowledged peer's would be, until the pin is dropped.
    pub(crate) fn pin(&self) -> Result<Pinned<'_, F>, EngineError> {
        let shared = &*self.shared;
        if shared.is_poisoned() {
            return Err(shared.refusal());
        }
        let durable = lock(&shared.durable);
        if durable.header.staged_lsn != 0 {
            if shared.read_only {
                return Err(EngineError::ReadOnly(
                    "pin a checkpoint that a crash cut between its stages, so open it once for writing",
                ));
            }
            return Err(EngineError::corrupt(
                "the last durable checkpoint wrote only some of its stages",
            ));
        }
        let header = Header {
            clean: true,
            ..durable.header
        };
        let extents = durable.extents.clone();
        let layers = Arc::new(Layers {
            active: Arc::new(Memtable::default()),
            frozen: None,
            tree: Arc::clone(&durable.tree),
        });
        let epoch = replica::epoch_at(&replica::load_epochs(&layers)?, header.checkpoint_lsn);
        let id = {
            let mut retention = lock(&shared.retention);
            let id = retention.next_pin;
            retention.next_pin += 1;
            retention.pins.insert(
                id,
                Pin {
                    tree: Arc::clone(&durable.tree),
                    lsn: header.checkpoint_lsn,
                    deadline: shared.now().checked_add(shared.options.pin_limit),
                },
            );
            id
        };
        drop(durable);
        Ok(Pinned {
            engine: self,
            id,
            epoch,
            header,
            extents,
        })
    }

    #[cfg(test)]
    /// Runs `run` while holding the lock a checkpoint holds across its merge and sync.
    pub(crate) fn while_checkpointing<T>(&self, run: impl FnOnce() -> T) -> T {
        let _durable = lock(&self.shared.durable);
        run()
    }

    #[cfg(test)]
    /// Moves the clock that snapshot time limits read forward by `by`.
    pub(crate) fn advance_clock(&self, by: Duration) {
        *lock(&self.shared.clock_ahead) += by;
    }

    #[cfg(test)]
    /// The LSN of the oldest record the log still holds.
    pub(crate) fn log_start(&self) -> u64 {
        lock(&self.shared.writer)
            .wal
            .as_ref()
            .map_or(0, Wal::first_lsn)
    }

    #[cfg(test)]
    pub(crate) fn extents(&self) -> ExtentTable {
        lock(&self.shared.durable).extents.clone()
    }

    #[cfg(test)]
    /// The header of the last durable checkpoint.
    pub(crate) fn header(&self) -> Header {
        lock(&self.shared.durable).header
    }
}

impl<F: FileSystem + 'static> Shared<F> {
    fn is_poisoned(&self) -> bool {
        self.poisoned.load(Ordering::Acquire)
    }

    fn poison_with(&self, error: &EngineError) {
        lock(&self.failure).get_or_insert((error.is_corrupt(), error.to_string()));
        self.poison();
    }

    fn refusal(&self) -> EngineError {
        match &*lock(&self.failure) {
            Some((true, cause)) => {
                EngineError::corrupt(format!("an earlier checkpoint failed: {cause}"))
            }
            _ => EngineError::Poisoned,
        }
    }

    fn poison(&self) {
        self.poisoned.store(true, Ordering::Release);
    }

    fn wait_for_checkpoint(&self) {
        let mut running = lock(&self.running);
        while *running {
            running = self
                .finished
                .wait(running)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    fn drop_segments(&self, writer: &mut Writer<F>) {
        let Some(sequence) = lock(&self.droppable).take() else {
            return;
        };
        if let Some(wal) = writer.wal.as_mut() {
            wal.drop_before(self.retained_from(wal, sequence));
            self.wal_bytes.store(wal.size(), Ordering::Relaxed);
            self.log_first
                .store(wal.first_sequence(), Ordering::Relaxed);
        }
    }

    fn retained_from(&self, wal: &Wal<F>, needed: u64) -> u64 {
        let mut retention = lock(&self.retention);
        retention.expire(self.now());
        let first = wal.first_lsn();
        retention.peers.retain(|_, lsn| *lsn + 1 >= first);
        let pinned = retention
            .oldest_pin()
            .map_or(needed, |lsn| wal.first_needed(lsn).min(needed));
        while let Some(oldest) = retention.oldest_peer() {
            let from = wal.first_needed(oldest);
            if from >= needed {
                break;
            }
            if wal.size_between(from, needed) <= self.options.max_retained_bytes {
                return from.min(pinned);
            }
            retention.peers.retain(|_, lsn| *lsn != oldest);
        }
        pinned
    }

    fn held_blocks(&self) -> u64 {
        let oldest = self.oldest_tree();
        let blocks: u64 = lock(&self.freed)
            .iter()
            .filter(|(generation, _)| **generation > oldest)
            .flat_map(|(_, freed)| freed.iter())
            .map(|(_, blocks)| u64::from(blocks.count()))
            .sum();
        let slots = lock(&self.quarantine)
            .values()
            .filter(|dropped| **dropped > oldest)
            .count() as u64;
        blocks + slots * u64::from(EXTENT_BLOCKS)
    }

    #[cfg(not(test))]
    fn now(&self) -> Instant {
        Instant::now()
    }

    #[cfg(test)]
    fn now(&self) -> Instant {
        Instant::now() + *lock(&self.clock_ahead)
    }

    fn retained_area(&self, header: &Header) -> Retained {
        let peers: Vec<(Vec<u8>, u64)> = lock(&self.retention)
            .peers
            .iter()
            .map(|(peer, lsn)| (peer.clone(), *lsn))
            .collect();
        let first = self.log_first.load(Ordering::Relaxed);
        Retained {
            from: if peers.is_empty() && first >= header.wal.first_segment {
                0
            } else {
                first
            },
            peers,
        }
    }

    fn reconsider_retention(&self) {
        let needed = self.needed.load(Ordering::Relaxed);
        lock(&self.droppable).get_or_insert(needed);
    }

    fn freeze(&self, writer: &mut Writer<F>) -> Option<Job> {
        let mut running = lock(&self.running);
        if *running || self.is_poisoned() {
            return None;
        }
        let wal = writer.wal.as_ref()?;
        let mut published = lock(&self.published);
        let layers = &published.view.layers;
        let frozen = Arc::clone(&layers.active);
        let tree = Arc::clone(&layers.tree);
        published.view.layers = Arc::new(Layers {
            active: Arc::new(Memtable::default()),
            frozen: Some(Arc::clone(&frozen)),
            tree,
        });
        *running = true;
        Some(Job {
            frozen,
            lsn: writer.last_lsn,
            hlc: writer.last_hlc,
            chain: writer.chain,
            first_segment: wal.first_needed(writer.last_lsn),
        })
    }

    fn run_checkpoint(&self, job: Job, clean: bool) {
        let mut finish = EndCheckpointOnDrop {
            shared: self,
            completed: false,
        };
        let result = self.write_checkpoint(&job, clean);
        match result {
            Ok(tree) => {
                let mut published = lock(&self.published);
                let active = Arc::clone(&published.view.layers.active);
                published.view.layers = Arc::new(Layers {
                    active,
                    frozen: None,
                    tree,
                });
                drop(published);
                self.needed.store(job.first_segment, Ordering::Relaxed);
                *lock(&self.droppable) = Some(job.first_segment);
                let trimmed = self.trim(&mut lock(&self.durable));
                if let Err(error) = trimmed {
                    self.poison_with(&error);
                } else if !self.keeping_free_space.load(Ordering::Acquire)
                    && self.holds_too_much_free_space()
                {
                    self.compact_unless_held();
                }
            }
            Err(error) => self.poison_with(&error),
        }
        finish.completed = true;
    }

    fn compact_unless_held(&self) {
        let durable = lock(&self.durable).tree.generation;
        if self.oldest_tree() < durable {
            self.compaction_due.store(true, Ordering::Release);
        } else {
            self.compaction_due.store(false, Ordering::Release);
            if let Err(error) = self.compact() {
                self.poison_with(&error);
            }
        }
    }

    fn compact_when_due(&self) {
        if !self.compaction_due.load(Ordering::Acquire) {
            return;
        }
        {
            let mut running = lock(&self.running);
            if *running {
                return;
            }
            if self.is_poisoned() {
                self.compaction_due.store(false, Ordering::Release);
                return;
            }
            *running = true;
        }
        let mut finish = EndCheckpointOnDrop {
            shared: self,
            completed: false,
        };
        if self.holds_too_much_free_space() {
            self.compact_unless_held();
        } else {
            self.compaction_due.store(false, Ordering::Release);
        }
        finish.completed = true;
    }

    fn holds_too_much_free_space(&self) -> bool {
        let Ok(file) = self.pages.file_bytes() else {
            return false;
        };
        let live = lock(&self.durable)
            .extents
            .entries
            .values()
            .map(|entry| u64::from(entry.live()))
            .sum::<u64>()
            * BLOCK as u64;
        let slack = self.options.reclaim_slack;
        let open_extent = u64::from(EXTENT_BLOCKS) * BLOCK as u64;
        let compacted = self.compacted_bytes.load(Ordering::Relaxed);
        file > live
            .saturating_mul(2)
            .saturating_add(slack)
            .saturating_add(open_extent)
            && file > compacted.saturating_add(live.max(slack))
    }

    fn write_checkpoint(&self, job: &Job, clean: bool) -> Result<Arc<Tree>, EngineError> {
        let mut durable = lock(&self.durable);
        self.trim(&mut durable)?;
        let changes = job.frozen.newest();
        if changes.is_empty() && !durable.header.catalog.is_null() && !self.can_move_down(&durable)
        {
            let header = Header {
                generation: durable.header.generation + 1,
                checkpoint_lsn: job.lsn,
                checkpoint_hlc: job.hlc,
                wal: WalIdentity {
                    first_segment: job.first_segment,
                    ..durable.header.wal
                },
                clean,
                staged_lsn: 0,
                replication: Replication {
                    checkpoint_chain: job.chain,
                    ..durable.header.replication
                },
                ..durable.header
            };
            self.pages
                .write_header(&header, &self.retained_area(&header))?;
            durable.header = header;
            return Ok(Arc::clone(&durable.tree));
        }
        let stage = self.stage_blocks(&durable.extents);
        let mut checkpointer = Checkpointer::new(
            &self.pages,
            self.extents_to_place(&durable.extents),
            job.lsn,
            durable.header.generation + 1,
        );
        let mut roots = durable.tree.roots.clone();
        for (table, changes) in &changes {
            let parts = match roots.get(table) {
                Some(_) => parts(changes, stage),
                None => vec![changes.as_slice()],
            };
            for part in parts {
                match checkpointer.merge_table(*table, roots.get(table).copied(), part)? {
                    Some(root) => roots.insert(*table, root),
                    None => roots.remove(table),
                };
                if checkpointer.freed() >= stage {
                    let finished = checkpointer.finish(roots.clone(), &durable.catalog)?;
                    let tree =
                        self.install(&mut durable, roots.clone(), finished, job, Stage::Partial)?;
                    self.publish_tree(tree);
                    checkpointer = Checkpointer::new(
                        &self.pages,
                        self.extents_to_place(&durable.extents),
                        job.lsn,
                        durable.header.generation + 1,
                    );
                }
            }
        }
        let finished = checkpointer.finish(roots.clone(), &durable.catalog)?;
        self.install(&mut durable, roots, finished, job, Stage::Whole { clean })
    }

    fn stage_blocks(&self, extents: &ExtentTable) -> u64 {
        let live: u64 = extents
            .entries
            .values()
            .map(|entry| u64::from(entry.live()))
            .sum();
        (live / STAGE_SHARE).max(self.options.stage_floor)
    }

    fn publish_tree(&self, tree: Arc<Tree>) {
        let mut published = lock(&self.published);
        let layers = &published.view.layers;
        published.view.layers = Arc::new(Layers {
            active: Arc::clone(&layers.active),
            frozen: layers.frozen.clone(),
            tree,
        });
    }

    fn install(
        &self,
        durable: &mut Durable,
        roots: BTreeMap<TableId, Root>,
        finished: Finished,
        job: &Job,
        stage: Stage,
    ) -> Result<Arc<Tree>, EngineError> {
        self.pages.sync()?;
        let generation = durable.header.generation + 1;
        let first = finished
            .catalog
            .first()
            .ok_or_else(|| EngineError::corrupt("a checkpoint wrote no catalog"))?;
        let previous = durable.header;
        let header = Header {
            generation,
            catalog: first.at,
            catalog_lsn: first.lsn,
            next_extent: finished.extents.next_extent,
            open: finished.extents.open_extents()?,
            minor: FORMAT_MINOR,
            ..match stage {
                Stage::Partial => Header {
                    clean: false,
                    staged_lsn: job.lsn,
                    ..previous
                },
                Stage::Whole { clean } => Header {
                    checkpoint_lsn: job.lsn,
                    checkpoint_hlc: job.hlc,
                    wal: WalIdentity {
                        first_segment: job.first_segment,
                        ..previous.wal
                    },
                    clean,
                    staged_lsn: 0,
                    replication: Replication {
                        checkpoint_chain: job.chain,
                        ..previous.replication
                    },
                    ..previous
                },
            }
        };
        self.pages
            .write_header(&header, &self.retained_area(&header))?;
        let tree = Arc::new(Tree::new(
            generation,
            roots,
            Arc::clone(&durable.tree.source),
        ));
        lock(&self.trees).push(Arc::downgrade(&tree));
        let mut quarantine = lock(&self.quarantine);
        for slot in finished.dropped {
            quarantine.insert(slot, generation);
        }
        drop(quarantine);
        let freed: Vec<(ExtentId, Blocks)> = durable
            .extents
            .entries
            .iter()
            .filter_map(|(id, before)| {
                let after = finished.extents.entries.get(id)?;
                let freed = before.used.without(&after.used);
                (!freed.is_empty()).then_some((*id, freed))
            })
            .collect();
        if !freed.is_empty() {
            lock(&self.freed).insert(generation, freed);
        }
        *durable = Durable {
            header,
            extents: finished.extents,
            catalog: finished.catalog,
            tree: Arc::clone(&tree),
        };
        Ok(tree)
    }

    fn oldest_tree(&self) -> u64 {
        if lock(&self.retention).expire(self.now()) {
            self.reconsider_retention();
        }
        let mut trees = lock(&self.trees);
        trees.retain(|tree| tree.strong_count() > 0);
        trees
            .iter()
            .filter_map(|tree| tree.upgrade().map(|tree| tree.generation))
            .min()
            .unwrap_or(u64::MAX)
    }

    fn reusable_slots(&self, extents: &ExtentTable, oldest: u64) -> BTreeSet<u64> {
        let mut quarantine = lock(&self.quarantine);
        quarantine.retain(|_, dropped| *dropped > oldest);
        extents
            .free_slots()
            .into_iter()
            .filter(|slot| !quarantine.contains_key(slot))
            .collect()
    }

    fn reusable_holes(&self, extents: &ExtentTable, oldest: u64) -> Vec<(ExtentId, Blocks)> {
        let mut freed = lock(&self.freed);
        freed.retain(|generation, _| *generation > oldest);
        let mut held: BTreeMap<ExtentId, Blocks> = BTreeMap::new();
        for (id, blocks) in freed.values().flatten() {
            let all = held.entry(*id).or_default();
            *all = all.with(blocks);
        }
        drop(freed);
        let mut holes: Vec<(u64, ExtentId, Blocks)> = extents
            .entries
            .iter()
            .filter_map(|(id, entry)| {
                let free = entry
                    .free()
                    .without(&held.get(id).copied().unwrap_or_default());
                (!free.is_empty()).then_some((entry.slot, *id, free))
            })
            .collect();
        holes.sort_unstable_by_key(|(slot, _, _)| *slot);
        holes.into_iter().map(|(_, id, free)| (id, free)).collect()
    }

    fn can_move_down(&self, durable: &Durable) -> bool {
        let mut extents = self.extents_to_place(&durable.extents);
        let open = extents.open;
        extents.move_down();
        extents.open != open
    }

    fn extents_to_place(&self, extents: &ExtentTable) -> ExtentTable {
        let oldest = self.oldest_tree();
        let mut extents = extents.clone();
        if self.keeping_free_space.load(Ordering::Acquire) {
            extents.reusable = BTreeSet::new();
            extents.holes = Vec::new();
            return extents;
        }
        extents.reusable = self.reusable_slots(&extents, oldest);
        extents.holes = self.reusable_holes(&extents, oldest);
        extents
    }

    fn trim(&self, durable: &mut Durable) -> Result<(), EngineError> {
        if self.keeping_free_space.load(Ordering::Acquire) {
            return Ok(());
        }
        let removable = self.reusable_slots(&durable.extents, self.oldest_tree());
        if let Some((slot, written)) = durable.extents.trim(&removable) {
            self.pages.truncate_after(slot, written)?;
            let top = durable.extents.next_slot;
            lock(&self.quarantine).retain(|slot, _| *slot < top);
        }
        Ok(())
    }

    fn compact(&self) -> Result<(), EngineError> {
        for _ in 0..2 {
            let mut durable = lock(&self.durable);
            self.trim(&mut durable)?;
            let generation = durable.header.generation + 1;
            let lsn = durable.header.checkpoint_lsn;
            let mut checkpointer = Checkpointer::new(
                &self.pages,
                self.extents_to_place(&durable.extents),
                lsn,
                generation,
            );
            checkpointer.evacuate();
            let old = Arc::clone(&durable.tree);
            let mut roots = BTreeMap::new();
            for table in old.roots.keys() {
                if let Some(root) = checkpointer.rebuild(*table, &old)? {
                    roots.insert(*table, root);
                }
            }
            let finished = checkpointer.finish(roots.clone(), &[])?;
            let job = Job {
                frozen: Arc::new(Memtable::default()),
                lsn,
                hlc: durable.header.checkpoint_hlc,
                chain: durable.header.replication.checkpoint_chain,
                first_segment: durable.header.wal.first_segment,
            };
            let tree = self.install(
                &mut durable,
                roots,
                finished,
                &job,
                Stage::Whole { clean: false },
            )?;
            drop(durable);
            drop(old);
            self.publish_tree(tree);
        }
        let mut durable = lock(&self.durable);
        self.trim(&mut durable)?;
        drop(durable);
        self.compacted_bytes
            .store(self.pages.file_bytes()?, Ordering::Relaxed);
        Ok(())
    }

    fn close(&self) {
        let mut writer = match self.writer.try_lock() {
            Ok(writer) => writer,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => return self.leave_the_log_to_a_leaked_writer(),
        };
        self.wait_for_checkpoint();
        if self.is_poisoned() {
            return;
        }
        let keep = writer.wal.as_ref().map_or(u64::MAX, |wal| {
            let keep = self.retained_from(wal, wal.last_sequence() + 1);
            self.log_first.store(keep, Ordering::Relaxed);
            keep
        });
        let Some(job) = self.freeze(&mut writer) else {
            return;
        };
        self.run_checkpoint(job, true);
        if self.is_poisoned() {
            return;
        }
        let unfinished = {
            let durable = lock(&self.durable);
            !durable.header.clean || self.can_move_down(&durable)
        };
        if unfinished && let Some(job) = self.freeze(&mut writer) {
            self.run_checkpoint(job, true);
            if self.is_poisoned() {
                return;
            }
        }
        if let Some(wal) = writer.wal.take() {
            let _ = wal.close_keeping(keep);
        }
        lock(&self.droppable).take();
    }
}

impl<F: FileSystem + 'static> Shared<F> {
    fn leave_the_log_to_a_leaked_writer(&self) {}
}

struct EndCheckpointOnDrop<'a, F: FileSystem + 'static> {
    shared: &'a Shared<F>,
    completed: bool,
}

impl<F: FileSystem + 'static> Drop for EndCheckpointOnDrop<'_, F> {
    fn drop(&mut self) {
        if !self.completed {
            self.shared.poison();
        }
        *lock(&self.shared.running) = false;
        self.shared.finished.notify_all();
    }
}

impl<F: FileSystem + 'static> Drop for Engine<F> {
    fn drop(&mut self) {
        self.stop_checkpoints();
        self.shared.close();
    }
}

/// A durable checkpoint that [`Engine::pin`] keeps whole: its header, written as a clean one,
/// and its extent table.
#[derive(Debug)]
pub(crate) struct Pinned<'a, F: FileSystem + 'static> {
    engine: &'a Engine<F>,
    id: u64,
    pub(crate) epoch: EpochId,
    pub(crate) header: Header,
    pub(crate) extents: ExtentTable,
}

impl<F: FileSystem + 'static> Pinned<'_, F> {
    /// Fills `buffer` with the file's blocks from block `first` on, as they are on disk.
    /// Fails once the pin outlived the engine's limit and was released, since its blocks may
    /// then hold newer pages.
    pub(crate) fn read(&self, first: u64, buffer: &mut [u8]) -> Result<(), EngineError> {
        let _held = self.hold()?;
        self.engine.shared.pages.read_blocks(first, buffer)
    }

    /// The pinned tree, which keeps its blocks while it is held, or a failure once the pin was
    /// released.
    pub(crate) fn hold(&self) -> Result<Arc<Tree>, EngineError> {
        let shared = &*self.engine.shared;
        let held = lock(&shared.retention)
            .pins
            .get(&self.id)
            .map(|pin| Arc::clone(&pin.tree));
        held.ok_or_else(|| {
            EngineError::Storage(StorageError::of_kind(
                ErrorKind::SnapshotExpired,
                io::Error::other(format!(
                    "the snapshot outlived its limit of {:?} and was released",
                    shared.options.pin_limit
                )),
            ))
        })
    }
}

impl<F: FileSystem + 'static> Drop for Pinned<'_, F> {
    fn drop(&mut self) {
        lock(&self.engine.shared.retention).pins.remove(&self.id);
        self.engine.shared.reconsider_retention();
    }
}

#[derive(Debug)]
pub(crate) struct EngineRead<'a, F: FileSystem + 'static> {
    engine: &'a Engine<F>,
    view: View,
}

impl<F: FileSystem + 'static> Drop for EngineRead<'_, F> {
    fn drop(&mut self) {
        self.engine.deregister(self.view.snapshot);
    }
}

/// The one write transaction of an [`Engine`], which holds the writer lock while it lives.
#[derive(Debug)]
pub(crate) struct EngineWrite<'a, F: FileSystem + 'static> {
    engine: &'a Engine<F>,
    writer: MutexGuard<'a, Writer<F>>,
    staged: Staged,
}

impl<F: FileSystem + 'static> Storage for Engine<F> {
    type Read<'a>
        = EngineRead<'a, F>
    where
        Self: 'a;
    type Write<'a>
        = EngineWrite<'a, F>
    where
        Self: 'a;

    fn begin_read(&self) -> Result<EngineRead<'_, F>, StorageError> {
        Ok(EngineRead {
            engine: self,
            view: self.register(),
        })
    }

    fn begin_write(&self) -> Result<EngineWrite<'_, F>, StorageError> {
        self.writable("write")?;
        let writer = lock(&self.shared.writer);
        if self.shared.is_poisoned() {
            return Err(self.shared.refusal().into());
        }
        let view = lock(&self.shared.published).view.clone();
        Ok(EngineWrite {
            engine: self,
            writer,
            staged: Staged::new(view),
        })
    }

    fn write_copy(
        snapshot: &EngineRead<'_, F>,
        path: &std::path::Path,
    ) -> Result<(), StorageError> {
        super::any_fs::write_copy(snapshot, path)
    }

    fn checkpoint(&self) -> Result<(), StorageError> {
        Ok(Engine::checkpoint_keeping_log(self)?)
    }
}

impl<F: FileSystem + 'static> Maintenance for Engine<F> {
    fn compact(&self) -> Result<(), StorageError> {
        Ok(Engine::compact(self)?)
    }

    fn stats(&self) -> Result<StorageStats, StorageError> {
        let shared = &*self.shared;
        let file_bytes = shared.pages.file_bytes()?;
        let (live_extents, live_blocks) = {
            let durable = lock(&shared.durable);
            let entries = &durable.extents.entries;
            let live = entries
                .values()
                .map(|entry| u64::from(entry.live()))
                .sum::<u64>();
            (entries.len() as u64, live)
        };
        let slots = (file_bytes / BLOCK as u64)
            .saturating_sub(2)
            .div_ceil(EXTENT_BLOCKS.into());
        let (cache_hits, cache_misses) = shared.pages.cache_hits_and_misses();
        Ok(StorageStats {
            file_bytes,
            live_bytes: live_blocks * BLOCK as u64,
            live_extents,
            free_extents: slots.saturating_sub(live_extents),
            wal_bytes: shared.wal_bytes.load(Ordering::Relaxed),
            cache_hits,
            cache_misses,
            held_bytes: shared.held_blocks() * BLOCK as u64,
        })
    }

    fn check(&self) -> Result<(), StorageError> {
        Engine::check(self)?;
        Ok(())
    }
}

fn prefix_bounds(prefix: &[u8]) -> (Bound<&[u8]>, Option<Vec<u8>>) {
    (Bound::Included(prefix), prefix_end(prefix))
}

fn end_bound(end: &Option<Vec<u8>>) -> Bound<&[u8]> {
    match end {
        Some(end) => Bound::Excluded(end),
        None => Bound::Unbounded,
    }
}

type Bounds<'a> = (Bound<&'a [u8]>, Bound<&'a [u8]>);

trait Reads {
    fn lookup<T>(
        &self,
        table: TableId,
        key: &[u8],
        read: impl FnOnce(&[u8]) -> T,
    ) -> Result<Option<T>, EngineError>;

    fn walk<E: From<StorageError>>(
        &self,
        table: TableId,
        start: Bound<&[u8]>,
        end: Bound<&[u8]>,
        visit: Visit<'_, E>,
    ) -> Result<(), E>;

    fn load(&self, blob: &BlobRef, access: Access) -> Result<Arc<[u8]>, EngineError>;
}

impl<F: FileSystem + 'static> Reads for EngineRead<'_, F> {
    fn lookup<T>(
        &self,
        table: TableId,
        key: &[u8],
        read: impl FnOnce(&[u8]) -> T,
    ) -> Result<Option<T>, EngineError> {
        self.view.get_with(table, key, read)
    }

    fn walk<E: From<StorageError>>(
        &self,
        table: TableId,
        start: Bound<&[u8]>,
        end: Bound<&[u8]>,
        visit: Visit<'_, E>,
    ) -> Result<(), E> {
        self.view.scan(table, start, end, visit)
    }

    fn load(&self, blob: &BlobRef, access: Access) -> Result<Arc<[u8]>, EngineError> {
        self.view.load(blob, access)
    }
}

impl<F: FileSystem + 'static> Reads for EngineWrite<'_, F> {
    fn lookup<T>(
        &self,
        table: TableId,
        key: &[u8],
        read: impl FnOnce(&[u8]) -> T,
    ) -> Result<Option<T>, EngineError> {
        self.staged.get_with(table, key, read)
    }

    fn walk<E: From<StorageError>>(
        &self,
        table: TableId,
        start: Bound<&[u8]>,
        end: Bound<&[u8]>,
        visit: Visit<'_, E>,
    ) -> Result<(), E> {
        self.staged.scan(table, start, end, visit)
    }

    fn load(&self, blob: &BlobRef, access: Access) -> Result<Arc<[u8]>, EngineError> {
        self.staged.view.load(blob, access)
    }
}

fn walk_values<R: Reads, E: From<StorageError>>(
    reads: &R,
    table: Table,
    (start, end): Bounds<'_>,
    mut visit: impl FnMut(&[u8], &[u8]) -> Result<(), E>,
) -> Result<(), E> {
    let mut loaded = 0;
    reads.walk(
        TableId::of(table),
        start,
        end,
        &mut |key, value| match value {
            CellValue::Inline(value) => visit(key, value),
            CellValue::External(blob) => {
                let access = Access::of_scan(&mut loaded, blob.length as usize);
                visit(key, &reads.load(&blob, access).map_err(StorageError::from)?)
            }
        },
    )
}

fn walk_peeks<R: Reads, E: From<StorageError>>(
    reads: &R,
    table: Table,
    (start, end): Bounds<'_>,
    mut visit: impl FnMut(&[u8], Peek<'_>) -> Result<(), E>,
) -> Result<(), E> {
    reads.walk(
        TableId::of(table),
        start,
        end,
        &mut |key, value| match value {
            CellValue::Inline(value) => visit(key, Peek::Bytes(value)),
            CellValue::External(blob) => match usize::try_from(blob.length) {
                Ok(length) => visit(key, Peek::Apart(length)),
                Err(_) => Err(StorageError::from(EngineError::TooLarge).into()),
            },
        },
    )
}

fn collect<R: Reads>(
    reads: &R,
    table: Table,
    bounds: Bounds<'_>,
) -> Result<Vec<Entry>, StorageError> {
    let mut entries = Vec::new();
    walk_values(reads, table, bounds, |key, value| {
        entries.push((key.to_vec(), value.to_vec()));
        Ok::<(), StorageError>(())
    })?;
    Ok(entries)
}

fn get<R: Reads>(reads: &R, table: Table, key: &[u8]) -> Result<Option<Vec<u8>>, StorageError> {
    Ok(reads.lookup(TableId::of(table), key, <[u8]>::to_vec)?)
}

fn get_with<R: Reads, T, E: From<StorageError>>(
    reads: &R,
    table: Table,
    key: &[u8],
    read: impl FnOnce(&[u8]) -> Result<T, E>,
) -> Result<Option<T>, E> {
    reads
        .lookup(TableId::of(table), key, read)
        .map_err(StorageError::from)?
        .transpose()
}

macro_rules! read_transaction {
    ($type:ident) => {
        impl<F: FileSystem + 'static> ReadTransaction for $type<'_, F> {
            fn get(&self, table: Table, key: &[u8]) -> Result<Option<Vec<u8>>, StorageError> {
                get(self, table, key)
            }

            fn scan_prefix(&self, table: Table, prefix: &[u8]) -> Result<Vec<Entry>, StorageError> {
                let (start, end) = prefix_bounds(prefix);
                collect(self, table, (start, end_bound(&end)))
            }

            fn scan_range(
                &self,
                table: Table,
                start: &[u8],
                end: &[u8],
            ) -> Result<Vec<Entry>, StorageError> {
                if start >= end {
                    return Ok(Vec::new());
                }
                collect(self, table, (Bound::Included(start), Bound::Excluded(end)))
            }

            fn get_with<R, E: From<StorageError>>(
                &self,
                table: Table,
                key: &[u8],
                read: impl FnOnce(&[u8]) -> Result<R, E>,
            ) -> Result<Option<R>, E> {
                get_with(self, table, key, read)
            }

            fn visit_prefix<E: From<StorageError>>(
                &self,
                table: Table,
                prefix: &[u8],
                visit: impl FnMut(&[u8], &[u8]) -> Result<(), E>,
            ) -> Result<(), E> {
                let (start, end) = prefix_bounds(prefix);
                walk_values(self, table, (start, end_bound(&end)), visit)
            }

            fn visit_range<E: From<StorageError>>(
                &self,
                table: Table,
                start: &[u8],
                end: &[u8],
                visit: impl FnMut(&[u8], &[u8]) -> Result<(), E>,
            ) -> Result<(), E> {
                if start >= end {
                    return Ok(());
                }
                let bounds = (Bound::Included(start), Bound::Excluded(end));
                walk_values(self, table, bounds, visit)
            }

            fn visit_prefix_peek<E: From<StorageError>>(
                &self,
                table: Table,
                prefix: &[u8],
                visit: impl FnMut(&[u8], Peek<'_>) -> Result<(), E>,
            ) -> Result<(), E> {
                let (start, end) = prefix_bounds(prefix);
                walk_peeks(self, table, (start, end_bound(&end)), visit)
            }

            fn visit_range_peek<E: From<StorageError>>(
                &self,
                table: Table,
                start: &[u8],
                end: &[u8],
                visit: impl FnMut(&[u8], Peek<'_>) -> Result<(), E>,
            ) -> Result<(), E> {
                if start >= end {
                    return Ok(());
                }
                let bounds = (Bound::Included(start), Bound::Excluded(end));
                walk_peeks(self, table, bounds, visit)
            }
        }
    };
}

read_transaction!(EngineRead);
read_transaction!(EngineWrite);

impl<F: FileSystem + 'static> WriteTransaction for EngineWrite<'_, F> {
    fn put(&mut self, table: Table, key: &[u8], value: &[u8]) -> Result<(), StorageError> {
        self.staged.put(TableId::of(table), key, value);
        Ok(())
    }

    fn delete(&mut self, table: Table, key: &[u8]) -> Result<(), StorageError> {
        self.staged.delete(TableId::of(table), key);
        Ok(())
    }

    fn delete_range(&mut self, table: Table, start: &[u8], end: &[u8]) -> Result<(), StorageError> {
        Ok(self.staged.delete_range(TableId::of(table), start, end)?)
    }

    fn commit(self) -> Result<(), StorageError> {
        let Self {
            engine,
            mut writer,
            staged,
        } = self;
        Ok(engine.commit(&mut writer, staged)?)
    }

    fn rollback(self) -> Result<(), StorageError> {
        Ok(())
    }
}

fn stage_record(staged: &mut Staged, lsn: u64, term: u64, body: Body) -> Result<(), EngineError> {
    match body {
        Body::Batch(ops) => {
            for op in ops {
                staged.replay(op)?;
            }
        }
        Body::Config {
            writer_id,
            incarnation,
            step,
        } => {
            let table = TableId::of(Table::ReplicationHistory);
            let key = term.to_be_bytes();
            let epoch = match step {
                Step::Begin => Epoch {
                    term,
                    writer_id,
                    start_lsn: lsn,
                    incarnation,
                    ended_lsn: 0,
                },
                Step::StepDown => {
                    let held = staged
                        .get_with(table, &key, |value| replica::decode_epoch(&key, value))?
                        .transpose()?;
                    match held {
                        Some(epoch)
                            if epoch.writer_id == writer_id
                                && (epoch.ended_lsn == 0 || epoch.ended_lsn == lsn) =>
                        {
                            Epoch {
                                ended_lsn: lsn,
                                ..epoch
                            }
                        }
                        _ => {
                            return Err(EngineError::corrupt(
                                "a writer stepped down from a term it does not hold",
                            ));
                        }
                    }
                }
            };
            let (key, value) = replica::history_entry(&epoch);
            staged.put(table, &key, &value);
        }
        Body::Checkpoint { .. } => {}
    }
    Ok(())
}

#[path = "replica.rs"]
pub(crate) mod replica;

pub(crate) use replica::Epoch;
pub(crate) use replica::Role;
pub(crate) use replica::different;
pub(crate) use replica::epoch_at;
pub(crate) use replica::fork_lsn;

#[cfg(test)]
#[path = "store.test.rs"]
mod tests;
