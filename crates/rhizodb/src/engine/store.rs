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
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;

use super::blob::RunRef;
use super::block_file::BlockFile;
use super::block_file::BlockSize;
use super::block_file::FileSystem;
use super::catalog::Root;
use super::check;
use super::check::CheckReport;
use super::checkpoint::Checkpointer;
use super::checkpoint::Finished;
use super::checkpoint::load_catalog;
use super::error::EngineError;
use super::extent::ExtentTable;
use super::header;
use super::header::FORMAT_MINOR;
use super::header::Header;
use super::header::WalIdentity;
use super::layout::BLOCK;
use super::layout::ExtentId;
use super::layout::TableId;
use super::memtable::Bytes;
use super::memtable::Memtable;
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
use super::wal::LogIdentity;
use super::wal::Wal;
use super::wal::segment_files;
use super::wal::segment_name;
use super::wal_format::Body;
use super::wal_format::batch_record;
use crate::storage::Entry;
use crate::storage::ReadTransaction;
use crate::storage::Storage;
use crate::storage::StorageError;
use crate::storage::Table;
use crate::storage::WriteTransaction;

/// Where checkpoints run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CheckpointMode {
    /// On a thread of the engine's own, while commits go on.
    Background,
    /// On the committing thread, so a test sees the same operations every run.
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
    /// The pages the block cache holds.
    pub(crate) cache_pages: usize,
}

impl Default for EngineOptions {
    fn default() -> Self {
        Self {
            segment_target: 16 << 20,
            memtable_limit: 64 << 20,
            wal_limit: 256 << 20,
            checkpoint: CheckpointMode::Background,
            cache_pages: (64 << 20) / BLOCK,
        }
    }
}

fn block_size() -> Result<BlockSize, EngineError> {
    BlockSize::new(BLOCK).ok_or(EngineError::TooLarge)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn random_u64() -> u64 {
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
    readers: BTreeMap<u64, usize>,
}

#[derive(Debug)]
struct Writer<F: FileSystem> {
    wal: Option<Wal<F>>,
    last_lsn: u64,
    last_hlc: u64,
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
    first_segment: u64,
}

#[derive(Debug)]
struct Shared<F: FileSystem> {
    options: EngineOptions,
    pages: Arc<Pages<F::File>>,
    published: Mutex<Published>,
    writer: Mutex<Writer<F>>,
    durable: Mutex<Durable>,
    droppable: Mutex<Option<u64>>,
    running: Mutex<bool>,
    finished: Condvar,
    failure: Mutex<Option<(bool, String)>>,
    trees: Mutex<Vec<Weak<Tree>>>,
    quarantine: Mutex<BTreeMap<u64, u64>>,
    poisoned: AtomicBool,
}

/// RhizoDB's own storage engine over one database file and its write-ahead log.
#[derive(Debug)]
pub(crate) struct Engine<F: FileSystem + 'static> {
    shared: Arc<Shared<F>>,
    jobs: Option<mpsc::Sender<Job>>,
    worker: Option<thread::JoinHandle<()>>,
}

fn read_header(file: &impl BlockFile) -> Result<Header, EngineError> {
    if file.block_count()? < 2 {
        return Err(EngineError::corrupt(
            "the database file is shorter than its two header slots",
        ));
    }
    let mut slots = [vec![0; BLOCK], vec![0; BLOCK]];
    for (slot, block) in slots.iter_mut().enumerate() {
        file.read(slot as u64, block)?;
    }
    header::choose(
        header::decode_slot(&slots[0], 0),
        header::decode_slot(&slots[1], 1),
    )
}

fn write_new_database<F: BlockFile>(file: &F) -> Result<(), EngineError> {
    let database_id = (u128::from(random_u64()) << 64) | u128::from(random_u64());
    let header = Header {
        minor: FORMAT_MINOR,
        database_id,
        generation: 1,
        next_extent: ExtentId(1),
        clean: true,
        ..Header::default()
    };
    file.set_block_count(0)?;
    file.write(0, &vec![0; BLOCK])?;
    file.write(header.slot(), &header.encode())?;
    file.sync()?;
    Ok(())
}

fn staging_name(name: &str) -> String {
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

fn open_or_create_staging<F: FileSystem>(
    fs: &F,
    staging: &str,
) -> Result<Option<F::File>, EngineError> {
    match fs.create(staging, block_size()?) {
        Ok(file) => Ok(Some(file)),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            match fs.open(staging, block_size()?) {
                Ok(file) => Ok(Some(file)),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(error.into()),
            }
        }
        Err(error) => Err(error.into()),
    }
}

fn open_or_create_whole_or_not_at_all<F: FileSystem>(
    fs: &F,
    name: &str,
) -> Result<F::File, EngineError> {
    let staging = staging_name(name);
    let deadline = Instant::now() + CREATION_WAIT;
    loop {
        if let Existing::Database(file) = existing_database(fs, name)? {
            file.lock()?;
            ignore_missing(fs.remove(&staging))?;
            return Ok(file);
        }
        if let Some(file) = open_or_create_staging(fs, &staging)? {
            match file.lock() {
                Ok(()) => {
                    if let Existing::Database(_) = existing_database(fs, name)? {
                        drop(file);
                        ignore_missing(fs.remove(&staging))?;
                        continue;
                    }
                    write_new_database(&file)?;
                    fs.rename(&staging, name)?;
                    fs.sync_names()?;
                    return Ok(file);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error.into()),
            }
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

fn replay_into(
    layers: &Arc<Layers>,
    last_lsn: &mut u64,
    last_hlc: &mut u64,
    record: super::wal_format::Record,
) -> Result<(), EngineError> {
    if let Body::Batch(ops) = record.body {
        let mut staged = Staged::new(View {
            snapshot: *last_lsn,
            layers: Arc::clone(layers),
        });
        for op in ops {
            staged.replay(op)?;
        }
        layers
            .active
            .apply(record.lsn, staged.into_changes(), record.lsn);
    }
    *last_lsn = record.lsn;
    *last_hlc = (*last_hlc).max(record.hlc);
    Ok(())
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
        let header = read_header(&file)?;
        let pages = Arc::new(Pages::new(
            file,
            page_seed(header.database_id),
            options.cache_pages,
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
            let tree = Tree {
                generation: header.generation,
                roots: BTreeMap::new(),
                source: Arc::clone(&source),
            };
            (tree, extents, Vec::new())
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
            let tree = Tree {
                generation: header.generation,
                roots: catalog.roots,
                source: Arc::clone(&source),
            };
            (tree, catalog.extents, runs)
        };
        let tree = Arc::new(tree);
        let layers = Arc::new(Layers {
            active: Arc::new(Memtable::default()),
            frozen: None,
            tree: Arc::clone(&tree),
        });
        let mut last_lsn = header.checkpoint_lsn;
        let mut last_hlc = header.checkpoint_hlc;
        let (wal, header) = if header.clean {
            for sequence in segment_files(&*fs, name)? {
                fs.remove(&segment_name(name, sequence))?;
            }
            fs.sync_names()?;
            let identity = LogIdentity {
                database_id: header.database_id,
                shard_id: header.shard_id,
                salt: random_u64(),
            };
            let sequence = header.wal.first_segment + 1;
            last_hlc = hybrid_clock_after(last_hlc);
            let wal = Wal::create(
                Arc::clone(&fs),
                name,
                identity,
                sequence,
                last_lsn + 1,
                last_hlc,
                options.segment_target,
            )?;
            let header = Header {
                generation: header.generation + 1,
                wal: WalIdentity {
                    first_segment: sequence,
                    salt: identity.salt,
                },
                clean: false,
                ..header
            };
            pages.write_header(&header)?;
            (wal, header)
        } else {
            let wal = Wal::recover(
                Arc::clone(&fs),
                name,
                &header,
                options.segment_target,
                |record| replay_into(&layers, &mut last_lsn, &mut last_hlc, record),
            )?;
            (wal, header)
        };
        let shared = Arc::new(Shared {
            options,
            pages,
            published: Mutex::new(Published {
                view: View {
                    snapshot: last_lsn,
                    layers,
                },
                readers: BTreeMap::new(),
            }),
            writer: Mutex::new(Writer {
                wal: Some(wal),
                last_lsn,
                last_hlc,
            }),
            trees: Mutex::new(vec![Arc::downgrade(&tree)]),
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
            poisoned: AtomicBool::new(false),
        });
        let (jobs, worker) = match options.checkpoint {
            CheckpointMode::Inline => (None, None),
            CheckpointMode::Background => {
                let (sender, receiver) = mpsc::channel::<Job>();
                let worker_shared = Arc::clone(&shared);
                let worker = thread::Builder::new()
                    .name("rhizodb-checkpoint".to_owned())
                    .spawn(move || {
                        for job in receiver {
                            worker_shared.run_checkpoint(job, false);
                        }
                    })?;
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
        if staged.ops.is_empty() {
            return Ok(());
        }
        shared.drop_segments(writer);
        let lsn = writer.last_lsn + 1;
        let hlc = hybrid_clock_after(writer.last_hlc);
        let record = batch_record(
            lsn,
            hlc,
            u128::from(lsn),
            staged.ops.iter().map(StagedOp::borrowed),
        )?;
        let wal = writer.wal.as_mut().ok_or(EngineError::Poisoned)?;
        if let Err(error) = wal.append(record, hlc) {
            shared.poison_with(&error);
            return Err(error);
        }
        writer.last_lsn = lsn;
        writer.last_hlc = hlc;
        let (layers, horizon) = {
            let published = lock(&shared.published);
            let snapshot = published.view.snapshot;
            let oldest = published.readers.keys().next().copied().unwrap_or(snapshot);
            (Arc::clone(&published.view.layers), oldest.min(snapshot))
        };
        layers.active.apply(lsn, staged.into_changes(), horizon);
        lock(&shared.published).view.snapshot = lsn;
        let wal_size = writer.wal.as_ref().map_or(0, Wal::size);
        let full = layers.active.bytes() > shared.options.memtable_limit
            || wal_size > shared.options.wal_limit;
        if full {
            self.start_checkpoint(writer);
            if layers.frozen.is_some() && layers.active.bytes() > 2 * shared.options.memtable_limit
            {
                shared.wait_for_checkpoint();
            }
        }
        Ok(())
    }

    fn start_checkpoint(&self, writer: &mut Writer<F>) {
        let Some(job) = self.shared.freeze(writer) else {
            return;
        };
        match &self.jobs {
            Some(jobs) => {
                if let Err(mpsc::SendError(job)) = jobs.send(job) {
                    self.shared.run_checkpoint(job, false);
                }
            }
            None => {
                self.shared.run_checkpoint(job, false);
                self.shared.drop_segments(writer);
            }
        }
    }

    /// Writes every commit so far into the database file and drops the log segments it no
    /// longer needs.
    pub(crate) fn checkpoint(&self) -> Result<(), EngineError> {
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

    /// Checks every invariant of the database file as the last durable checkpoint left it.
    pub(crate) fn check(&self) -> Result<CheckReport, EngineError> {
        let durable = lock(&self.shared.durable);
        check::check(
            &durable.header,
            &durable.extents,
            &durable.catalog,
            &durable.tree,
        )
    }

    /// Writes every commit into the database file, copies every table into as few extents as
    /// they need, and cuts the file after them.
    pub(crate) fn compact(&self) -> Result<(), EngineError> {
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

    #[cfg(test)]
    pub(crate) fn extents(&self) -> ExtentTable {
        lock(&self.shared.durable).extents.clone()
    }

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
        if let Some(wal) = writer.wal.as_mut()
            && wal.drop_before(sequence).is_err()
        {
            self.poison();
        }
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
                *lock(&self.droppable) = Some(job.first_segment);
            }
            Err(error) => self.poison_with(&error),
        }
        finish.completed = true;
    }

    fn write_checkpoint(&self, job: &Job, clean: bool) -> Result<Arc<Tree>, EngineError> {
        let mut durable = lock(&self.durable);
        self.trim(&mut durable)?;
        let changes = job.frozen.newest();
        if changes.is_empty() && !durable.header.catalog.is_null() {
            let header = Header {
                generation: durable.header.generation + 1,
                checkpoint_lsn: job.lsn,
                checkpoint_hlc: job.hlc,
                wal: WalIdentity {
                    first_segment: job.first_segment,
                    ..durable.header.wal
                },
                clean,
                ..durable.header
            };
            self.pages.write_header(&header)?;
            durable.header = header;
            return Ok(Arc::clone(&durable.tree));
        }
        let generation = durable.header.generation + 1;
        let mut checkpointer = Checkpointer::new(
            &self.pages,
            self.extents_to_place(&durable.extents),
            job.lsn,
            generation,
        );
        let mut roots = durable.tree.roots.clone();
        for (table, changes) in &changes {
            match checkpointer.merge_table(*table, roots.get(table).copied(), changes)? {
                Some(root) => roots.insert(*table, root),
                None => roots.remove(table),
            };
        }
        let finished = checkpointer.finish(roots.clone(), &durable.catalog)?;
        self.install(&mut durable, roots, finished, job, clean)
    }

    fn install(
        &self,
        durable: &mut Durable,
        roots: BTreeMap<TableId, Root>,
        finished: Finished,
        job: &Job,
        clean: bool,
    ) -> Result<Arc<Tree>, EngineError> {
        self.pages.sync()?;
        let generation = durable.header.generation + 1;
        let first = finished
            .catalog
            .first()
            .ok_or_else(|| EngineError::corrupt("a checkpoint wrote no catalog"))?;
        let header = Header {
            generation,
            checkpoint_lsn: job.lsn,
            checkpoint_hlc: job.hlc,
            catalog: first.at,
            catalog_lsn: first.lsn,
            next_extent: finished.extents.next_extent,
            open: finished.extents.open_extents()?,
            wal: WalIdentity {
                first_segment: job.first_segment,
                ..durable.header.wal
            },
            clean,
            ..durable.header
        };
        self.pages.write_header(&header)?;
        let tree = Arc::new(Tree {
            generation,
            roots,
            source: Arc::clone(&durable.tree.source),
        });
        lock(&self.trees).push(Arc::downgrade(&tree));
        let mut quarantine = lock(&self.quarantine);
        for slot in finished.dropped {
            quarantine.insert(slot, generation);
        }
        drop(quarantine);
        *durable = Durable {
            header,
            extents: finished.extents,
            catalog: finished.catalog,
            tree: Arc::clone(&tree),
        };
        Ok(tree)
    }

    fn oldest_tree(&self) -> u64 {
        let mut trees = lock(&self.trees);
        trees.retain(|tree| tree.strong_count() > 0);
        trees
            .iter()
            .filter_map(|tree| tree.upgrade().map(|tree| tree.generation))
            .min()
            .unwrap_or(u64::MAX)
    }

    fn reusable_slots(&self, extents: &ExtentTable) -> BTreeSet<u64> {
        let oldest = self.oldest_tree();
        let mut quarantine = lock(&self.quarantine);
        quarantine.retain(|_, dropped| *dropped > oldest);
        extents
            .free_slots()
            .into_iter()
            .filter(|slot| !quarantine.contains_key(slot))
            .collect()
    }

    fn extents_to_place(&self, extents: &ExtentTable) -> ExtentTable {
        let mut extents = extents.clone();
        extents.reusable = self.reusable_slots(&extents);
        extents
    }

    fn trim(&self, durable: &mut Durable) -> Result<(), EngineError> {
        let removable = self.reusable_slots(&durable.extents);
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
                first_segment: durable.header.wal.first_segment,
            };
            let tree = self.install(&mut durable, roots, finished, &job, false)?;
            drop(durable);
            drop(old);
            let mut published = lock(&self.published);
            let layers = &published.view.layers;
            published.view.layers = Arc::new(Layers {
                active: Arc::clone(&layers.active),
                frozen: layers.frozen.clone(),
                tree,
            });
        }
        let mut durable = lock(&self.durable);
        self.trim(&mut durable)
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
        let Some(job) = self.freeze(&mut writer) else {
            return;
        };
        self.run_checkpoint(job, true);
        if self.is_poisoned() {
            return;
        }
        if let Some(wal) = writer.wal.take() {
            let _ = wal.remove_all();
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
        drop(self.jobs.take());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        self.shared.close();
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

fn collect(
    scan: impl FnOnce(Visit<'_, StorageError>) -> Result<(), StorageError>,
) -> Result<Vec<Entry>, StorageError> {
    let mut entries = Vec::new();
    scan(&mut |key, value| {
        entries.push((key.to_vec(), value.to_vec()));
        Ok(())
    })?;
    Ok(entries)
}

trait Reads {
    fn lookup(&self, table: TableId, key: &[u8]) -> Result<Option<Bytes>, EngineError>;

    fn walk<E: From<StorageError>>(
        &self,
        table: TableId,
        start: Bound<&[u8]>,
        end: Bound<&[u8]>,
        visit: Visit<'_, E>,
    ) -> Result<(), E>;
}

impl<F: FileSystem + 'static> Reads for EngineRead<'_, F> {
    fn lookup(&self, table: TableId, key: &[u8]) -> Result<Option<Bytes>, EngineError> {
        self.view.get(table, key)
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
}

impl<F: FileSystem + 'static> Reads for EngineWrite<'_, F> {
    fn lookup(&self, table: TableId, key: &[u8]) -> Result<Option<Bytes>, EngineError> {
        self.staged.get(table, key)
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
}

fn get<R: Reads>(reads: &R, table: Table, key: &[u8]) -> Result<Option<Vec<u8>>, StorageError> {
    Ok(reads
        .lookup(TableId::of(table), key)?
        .map(|value| value.to_vec()))
}

fn scan_prefix<R: Reads>(
    reads: &R,
    table: Table,
    prefix: &[u8],
) -> Result<Vec<Entry>, StorageError> {
    let (start, end) = prefix_bounds(prefix);
    collect(|visit| reads.walk(TableId::of(table), start, end_bound(&end), visit))
}

fn scan_range<R: Reads>(
    reads: &R,
    table: Table,
    start: &[u8],
    end: &[u8],
) -> Result<Vec<Entry>, StorageError> {
    if start >= end {
        return Ok(Vec::new());
    }
    collect(|visit| {
        reads.walk(
            TableId::of(table),
            Bound::Included(start),
            Bound::Excluded(end),
            visit,
        )
    })
}

fn get_with<R: Reads, T, E: From<StorageError>>(
    reads: &R,
    table: Table,
    key: &[u8],
    read: impl FnOnce(&[u8]) -> Result<T, E>,
) -> Result<Option<T>, E> {
    match reads
        .lookup(TableId::of(table), key)
        .map_err(StorageError::from)?
    {
        Some(value) => read(&value).map(Some),
        None => Ok(None),
    }
}

fn visit_prefix<R: Reads, E: From<StorageError>>(
    reads: &R,
    table: Table,
    prefix: &[u8],
    mut visit: impl FnMut(&[u8], &[u8]) -> Result<(), E>,
) -> Result<(), E> {
    let (start, end) = prefix_bounds(prefix);
    reads.walk(TableId::of(table), start, end_bound(&end), &mut visit)
}

fn visit_range<R: Reads, E: From<StorageError>>(
    reads: &R,
    table: Table,
    start: &[u8],
    end: &[u8],
    mut visit: impl FnMut(&[u8], &[u8]) -> Result<(), E>,
) -> Result<(), E> {
    if start >= end {
        return Ok(());
    }
    reads.walk(
        TableId::of(table),
        Bound::Included(start),
        Bound::Excluded(end),
        &mut visit,
    )
}

macro_rules! read_transaction {
    ($type:ident) => {
        impl<F: FileSystem + 'static> ReadTransaction for $type<'_, F> {
            fn get(&self, table: Table, key: &[u8]) -> Result<Option<Vec<u8>>, StorageError> {
                get(self, table, key)
            }

            fn scan_prefix(&self, table: Table, prefix: &[u8]) -> Result<Vec<Entry>, StorageError> {
                scan_prefix(self, table, prefix)
            }

            fn scan_range(
                &self,
                table: Table,
                start: &[u8],
                end: &[u8],
            ) -> Result<Vec<Entry>, StorageError> {
                scan_range(self, table, start, end)
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
                visit_prefix(self, table, prefix, visit)
            }

            fn visit_range<E: From<StorageError>>(
                &self,
                table: Table,
                start: &[u8],
                end: &[u8],
                visit: impl FnMut(&[u8], &[u8]) -> Result<(), E>,
            ) -> Result<(), E> {
                visit_range(self, table, start, end, visit)
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

#[cfg(test)]
#[path = "store.test.rs"]
mod tests;
