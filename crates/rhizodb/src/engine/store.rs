use std::collections::BTreeMap;
use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::hash::Hasher;
use std::io;
use std::ops::Bound;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::SystemTime;

use super::block_file::BlockFile;
use super::block_file::BlockSize;
use super::block_file::FileSystem;
use super::error::EngineError;
use super::header;
use super::header::FORMAT_MINOR;
use super::header::Header;
use super::header::WalIdentity;
use super::layout::BLOCK;
use super::layout::ExtentId;
use super::layout::TableId;
use super::memtable::Bytes;
use super::memtable::Memtable;
use super::memtable::Slot;
use super::memtable::bounds_are_ordered;
use super::tree::Tree;
use super::view::Layers;
use super::view::View;
use super::view::Visit;
use super::view::merge_over;
use super::view::prefix_end;
use super::wal::LogIdentity;
use super::wal::Wal;
use super::wal::segment_files;
use super::wal::segment_name;
use super::wal_format::Body;
use super::wal_format::Op;
use super::wal_format::OpRef;
use super::wal_format::batch_record;
use crate::storage::Entry;
use crate::storage::ReadTransaction;
use crate::storage::Storage;
use crate::storage::StorageError;
use crate::storage::Table;
use crate::storage::WriteTransaction;

/// How the engine is tuned. The defaults suit a database on disk.
#[derive(Clone, Copy, Debug)]
pub(crate) struct EngineOptions {
    /// The size a log segment grows to before the next record starts a new one.
    pub(crate) segment_target: u64,
}

impl Default for EngineOptions {
    fn default() -> Self {
        Self {
            segment_target: 16 << 20,
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
    wal: Wal<F>,
    last_lsn: u64,
    last_hlc: u64,
}

#[derive(Debug)]
struct Shared<F: FileSystem> {
    published: Mutex<Published>,
    writer: Mutex<Writer<F>>,
    poisoned: AtomicBool,
}

/// RhizoDB's own storage engine over one database file and its write-ahead log.
#[derive(Debug)]
pub(crate) struct Engine<F: FileSystem> {
    shared: Shared<F>,
}

fn write_header(file: &impl BlockFile, header: &Header) -> Result<(), EngineError> {
    file.write(header.slot(), &header.encode())?;
    file.sync()?;
    Ok(())
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

fn create_file_whole_or_not_at_all<F: FileSystem>(
    fs: &F,
    name: &str,
    init: &str,
) -> Result<F::File, EngineError> {
    let file = fs.create(init, block_size()?)?;
    let database_id = (u128::from(random_u64()) << 64) | u128::from(random_u64());
    let header = Header {
        minor: FORMAT_MINOR,
        database_id,
        generation: 1,
        next_extent: ExtentId(1),
        clean: true,
        ..Header::default()
    };
    file.write(0, &vec![0; BLOCK])?;
    file.write(header.slot(), &header.encode())?;
    file.sync()?;
    fs.rename(init, name)?;
    fs.sync_names()?;
    Ok(file)
}

impl<F: FileSystem> Engine<F> {
    /// Opens the database file `name` on `fs`, creating it when there is none, and replays the
    /// log of an unclean shutdown before it returns.
    pub(crate) fn open(
        fs: Arc<F>,
        name: &str,
        options: EngineOptions,
    ) -> Result<Self, EngineError> {
        let init = format!("{name}-init");
        match fs.remove(&init) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error.into()),
            _ => {}
        }
        let file = match fs.open(name, block_size()?) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                create_file_whole_or_not_at_all(&*fs, name, &init)?
            }
            Err(error) => return Err(error.into()),
        };
        let header = read_header(&file)?;
        let active = Arc::new(Memtable::default());
        let layers = Arc::new(Layers {
            active: Arc::clone(&active),
            frozen: None,
            tree: Arc::new(Tree),
        });
        let mut last_lsn = header.checkpoint_lsn;
        let mut last_hlc = header.checkpoint_hlc;
        let wal = if header.clean {
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
            write_header(&file, &header)?;
            wal
        } else {
            Wal::recover(
                Arc::clone(&fs),
                name,
                &header,
                options.segment_target,
                |record| {
                    if let Body::Batch(ops) = record.body {
                        let mut staged = Staged::new(View {
                            snapshot: last_lsn,
                            layers: Arc::clone(&layers),
                        });
                        for op in ops {
                            staged.replay(op)?;
                        }
                        active.apply(record.lsn, staged.into_changes(), record.lsn);
                    }
                    last_lsn = record.lsn;
                    last_hlc = last_hlc.max(record.hlc);
                    Ok(())
                },
            )?
        };
        Ok(Self {
            shared: Shared {
                published: Mutex::new(Published {
                    view: View {
                        snapshot: last_lsn,
                        layers,
                    },
                    readers: BTreeMap::new(),
                }),
                writer: Mutex::new(Writer {
                    wal,
                    last_lsn,
                    last_hlc,
                }),
                poisoned: AtomicBool::new(false),
            },
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

    fn poison(&self) {
        self.shared.poisoned.store(true, Ordering::Release);
    }

    fn commit(&self, writer: &mut Writer<F>, staged: Staged) -> Result<(), EngineError> {
        if self.shared.poisoned.load(Ordering::Acquire) {
            return Err(EngineError::Poisoned);
        }
        if staged.ops.is_empty() {
            return Ok(());
        }
        let lsn = writer.last_lsn + 1;
        let hlc = hybrid_clock_after(writer.last_hlc);
        let record = batch_record(
            lsn,
            hlc,
            u128::from(lsn),
            staged.ops.iter().map(StagedOp::borrowed),
        )?;
        if let Err(error) = writer.wal.append(record, hlc) {
            self.poison();
            return Err(error);
        }
        writer.last_lsn = lsn;
        writer.last_hlc = hlc;
        let (layers, horizon) = {
            let published = lock(&self.shared.published);
            let snapshot = published.view.snapshot;
            let oldest = published.readers.keys().next().copied().unwrap_or(snapshot);
            (Arc::clone(&published.view.layers), oldest.min(snapshot))
        };
        layers.active.apply(lsn, staged.into_changes(), horizon);
        lock(&self.shared.published).view = View {
            snapshot: lsn,
            layers,
        };
        Ok(())
    }
}

#[derive(Debug)]
enum StagedOp {
    Put {
        table: TableId,
        key: Bytes,
        value: Bytes,
    },
    Delete {
        table: TableId,
        key: Bytes,
    },
    DeleteRange {
        table: TableId,
        start: Bytes,
        end: Bytes,
    },
}

impl StagedOp {
    fn borrowed(&self) -> OpRef<'_> {
        match self {
            StagedOp::Put { table, key, value } => OpRef::Put {
                table: *table,
                key,
                value,
            },
            StagedOp::Delete { table, key } => OpRef::Delete { table: *table, key },
            StagedOp::DeleteRange { table, start, end } => OpRef::DeleteRange {
                table: *table,
                start,
                end,
            },
        }
    }
}

#[derive(Debug)]
struct Staged {
    view: View,
    overlay: BTreeMap<TableId, BTreeMap<Bytes, Slot>>,
    ops: Vec<StagedOp>,
}

impl Staged {
    fn new(view: View) -> Self {
        Self {
            view,
            overlay: BTreeMap::new(),
            ops: Vec::new(),
        }
    }

    fn put(&mut self, table: TableId, key: &[u8], value: &[u8]) {
        let key: Bytes = Arc::from(key);
        let value: Bytes = Arc::from(value);
        self.overlay
            .entry(table)
            .or_default()
            .insert(Arc::clone(&key), Some(Arc::clone(&value)));
        self.ops.push(StagedOp::Put { table, key, value });
    }

    fn delete(&mut self, table: TableId, key: &[u8]) {
        let key: Bytes = Arc::from(key);
        self.overlay
            .entry(table)
            .or_default()
            .insert(Arc::clone(&key), None);
        self.ops.push(StagedOp::Delete { table, key });
    }

    fn delete_range(
        &mut self,
        table: TableId,
        start: &[u8],
        end: &[u8],
    ) -> Result<(), EngineError> {
        if start >= end {
            return Ok(());
        }
        let mut doomed = Vec::new();
        let bounds = (Bound::Included(start), Bound::Excluded(end));
        self.scan(table, bounds.0, bounds.1, &mut |key, _| {
            doomed.push(Arc::<[u8]>::from(key));
            Ok::<_, StorageError>(())
        })?;
        let overlay = self.overlay.entry(table).or_default();
        for key in doomed {
            overlay.insert(key, None);
        }
        self.ops.push(StagedOp::DeleteRange {
            table,
            start: Arc::from(start),
            end: Arc::from(end),
        });
        Ok(())
    }

    fn replay(&mut self, op: Op) -> Result<(), EngineError> {
        match op {
            Op::Put { table, key, value } => {
                table.table()?;
                self.put(table, &key, &value);
            }
            Op::Delete { table, key } => {
                table.table()?;
                self.delete(table, &key);
            }
            Op::DeleteRange { table, start, end } => {
                table.table()?;
                self.delete_range(table, &start, &end)?;
            }
        }
        Ok(())
    }

    fn get(&self, table: TableId, key: &[u8]) -> Result<Option<Bytes>, EngineError> {
        match self.overlay.get(&table).and_then(|keys| keys.get(key)) {
            Some(slot) => Ok(slot.clone()),
            None => self.view.get(table, key),
        }
    }

    fn scan<E: From<StorageError>>(
        &self,
        table: TableId,
        start: Bound<&[u8]>,
        end: Bound<&[u8]>,
        visit: Visit<'_, E>,
    ) -> Result<(), E> {
        let upper: Vec<(Bytes, Slot)> = match self.overlay.get(&table) {
            Some(keys) if bounds_are_ordered(start, end) => keys
                .range::<[u8], _>((start, end))
                .map(|(key, slot)| (Arc::clone(key), slot.clone()))
                .collect(),
            _ => Vec::new(),
        };
        merge_over(
            &upper,
            |lower| self.view.scan(table, start, end, lower),
            visit,
        )
    }

    fn into_changes(self) -> impl Iterator<Item = (TableId, Bytes, Slot)> {
        self.overlay
            .into_iter()
            .flat_map(|(table, keys)| keys.into_iter().map(move |(key, slot)| (table, key, slot)))
    }
}

/// A snapshot read of an [`Engine`].
#[derive(Debug)]
pub(crate) struct EngineRead<'a, F: FileSystem> {
    engine: &'a Engine<F>,
    view: View,
}

impl<F: FileSystem> Drop for EngineRead<'_, F> {
    fn drop(&mut self) {
        self.engine.deregister(self.view.snapshot);
    }
}

/// The one write transaction of an [`Engine`], which holds the writer lock while it lives.
#[derive(Debug)]
pub(crate) struct EngineWrite<'a, F: FileSystem> {
    engine: &'a Engine<F>,
    writer: MutexGuard<'a, Writer<F>>,
    staged: Staged,
}

impl<F: FileSystem> Storage for Engine<F> {
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
        if self.shared.poisoned.load(Ordering::Acquire) {
            return Err(EngineError::Poisoned.into());
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

impl<F: FileSystem> Reads for EngineRead<'_, F> {
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

impl<F: FileSystem> Reads for EngineWrite<'_, F> {
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

macro_rules! read_transaction {
    ($type:ident) => {
        impl<F: FileSystem> ReadTransaction for $type<'_, F> {
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
        }
    };
}

read_transaction!(EngineRead);
read_transaction!(EngineWrite);

impl<F: FileSystem> WriteTransaction for EngineWrite<'_, F> {
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
