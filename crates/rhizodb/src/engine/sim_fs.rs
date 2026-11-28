use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::io;
use std::mem;
use std::ops::Range;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;
use std::sync::atomic::AtomicU64;

use rhizodb_shm::Lock;

use super::block_file::BlockFile;
use super::block_file::BlockSize;
use super::block_file::FileSystem;
use super::block_file::SECTOR_SIZE;
use super::mem_fs::byte_index;
use super::mem_fs::read_blocks;
use super::mem_fs::resize_blocks;
use super::mem_fs::write_blocks;
use super::session::SideFile;
use super::session::SideLock;
use super::session::SideWords;
use super::session::WORDS;

const DEVICE_UNIT: usize = 4096;

/// A deterministic simulated file system that tells what was synced from what was only
/// written, so a test can build the files a power loss could leave and inject I/O errors.
/// It holds any number of files, so a layout spread over several of them can be tested.
#[derive(Clone, Debug, Default)]
pub(crate) struct SimFs {
    state: Arc<Mutex<State>>,
}

/// A file of [`SimFs`]. It keeps working after its name is removed, as on POSIX.
#[derive(Debug)]
pub(crate) struct SimFile {
    state: Arc<Mutex<State>>,
    inode: usize,
    size: BlockSize,
    handle: u64,
}

/// Which of the writes since the last sync a simulated power loss keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Crash {
    /// Only synced blocks, lengths, and names survive.
    DropUnsynced,
    /// Every write reached the disk.
    KeepUnsynced,
    /// The seed picks, for each block of each unsynced write, whether it is lost, kept, or
    /// torn at sectors into new, old, and garbage bytes, and a torn block may also garble
    /// other sectors of its 4 KiB unit, synced ones included. It applies the kept writes in a
    /// shuffled order, may lose an unsynced change of length, and keeps a prefix of the
    /// unsynced name changes. Writes a failed sync left behind are treated the same, before
    /// the writes after it.
    Scramble { seed: u64 },
    /// Like [`Crash::Scramble`], but each sector holds its old or its new bytes, never
    /// garbage, as on a device whose sector writes are atomic.
    Reorder { seed: u64 },
}

/// How long an injected I/O error lasts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Failure {
    /// Only the chosen operation fails.
    Once,
    /// The chosen operation and every later one fail.
    FromThenOn,
}

#[derive(Clone, Debug, Default)]
struct State {
    inodes: Vec<Inode>,
    live_names: BTreeMap<String, usize>,
    durable_names: BTreeMap<String, usize>,
    pending_names: Vec<NameChange>,
    names_sync_failed: bool,
    operations: u64,
    fault: Option<Fault>,
    capacity: Option<u64>,
    handles: u64,
    locks: BTreeMap<usize, Held>,
    sessions: BTreeMap<usize, BTreeSet<u64>>,
    sides: BTreeMap<usize, Arc<SimSide>>,
    refuse_sides: bool,
    unshared_sides: bool,
}

// ast-grep-ignore
// As Windows: exclusive blocks other handles reading and writing; shared blocks all writes.
#[derive(Clone, Debug)]
enum Held {
    Exclusive(u64),
    Shared(BTreeSet<u64>),
}

#[derive(Clone, Debug, Default)]
struct Inode {
    live: Vec<u8>,
    durable: Vec<u8>,
    pending: Vec<DataChange>,
    unsettled: Vec<DataChange>,
}

#[derive(Clone, Debug)]
enum DataChange {
    Write { start: usize, data: Vec<u8> },
    SetLength(usize),
}

#[derive(Clone, Debug)]
enum NameChange {
    Create { name: String, inode: usize },
    Rename { from: String, to: String },
    Remove { name: String },
}

#[derive(Clone, Copy, Debug)]
struct Fault {
    remaining: u64,
    failure: Failure,
    inode: Option<usize>,
    syncs_only: bool,
    failing: bool,
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

fn injected() -> io::Error {
    io::Error::other("injected I/O error")
}

fn not_found(name: &str) -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, format!("no file named {name}"))
}

fn lock_violation() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "another handle holds the lock on the simulated file",
    )
}

fn held_elsewhere() -> io::Error {
    io::Error::new(
        io::ErrorKind::WouldBlock,
        "another handle holds the lock on the simulated file",
    )
}

impl State {
    fn readable_by(&self, inode: usize, handle: u64) -> io::Result<()> {
        match self.locks.get(&inode) {
            Some(Held::Exclusive(holder)) if *holder != handle => Err(lock_violation()),
            _ => Ok(()),
        }
    }

    fn writable_by(&self, inode: usize, handle: u64) -> io::Result<()> {
        match self.locks.get(&inode) {
            Some(Held::Exclusive(holder)) if *holder != handle => Err(lock_violation()),
            Some(Held::Shared(_)) => Err(lock_violation()),
            _ => Ok(()),
        }
    }

    fn operate(&mut self, inode: Option<usize>, sync: bool) -> io::Result<()> {
        self.operations += 1;
        let Some(fault) = &mut self.fault else {
            return Ok(());
        };
        if fault.inode.is_some() && fault.inode != inode {
            return Ok(());
        }
        if fault.failing {
            return Err(injected());
        }
        if fault.syncs_only && !sync {
            return Ok(());
        }
        if fault.remaining > 0 {
            fault.remaining -= 1;
            return Ok(());
        }
        match fault.failure {
            Failure::Once => self.fault = None,
            Failure::FromThenOn => fault.failing = true,
        }
        Err(injected())
    }

    fn fits(&self, inode: usize, length: usize) -> io::Result<()> {
        let Some(capacity) = self.capacity else {
            return Ok(());
        };
        let growth = length.saturating_sub(self.inodes[inode].live.len());
        let used: usize = self
            .live_names
            .values()
            .map(|named| self.inodes[*named].live.len())
            .sum();
        if growth > 0 && (used + growth) as u64 > capacity {
            return Err(io::Error::new(
                io::ErrorKind::StorageFull,
                "no space left on the simulated device",
            ));
        }
        Ok(())
    }

    fn inode(&mut self, inode: usize) -> &mut Inode {
        &mut self.inodes[inode]
    }

    fn apply(names: &mut BTreeMap<String, usize>, change: &NameChange) {
        match change {
            NameChange::Create { name, inode } => {
                names.insert(name.clone(), *inode);
            }
            NameChange::Rename { from, to } => {
                if let Some(inode) = names.remove(from) {
                    names.insert(to.clone(), inode);
                }
            }
            NameChange::Remove { name } => {
                names.remove(name);
            }
        }
    }

    fn change_names(&mut self, change: NameChange) {
        Self::apply(&mut self.live_names, &change);
        self.pending_names.push(change);
    }
}

impl SimFs {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Makes the `operations`-th next operation fail, counting every call on the file system
    /// and its files.
    pub(crate) fn fail_after(&self, operations: u64, failure: Failure) {
        lock(&self.state).fault = Some(Fault {
            remaining: operations,
            failure,
            inode: None,
            syncs_only: false,
            failing: false,
        });
    }

    /// Like [`SimFs::fail_after`], counting only the calls on the file named `name`.
    pub(crate) fn fail_file_after(
        &self,
        name: &str,
        operations: u64,
        failure: Failure,
    ) -> io::Result<()> {
        let mut state = lock(&self.state);
        let inode = *state.live_names.get(name).ok_or_else(|| not_found(name))?;
        state.fault = Some(Fault {
            remaining: operations,
            failure,
            inode: Some(inode),
            syncs_only: false,
            failing: false,
        });
        Ok(())
    }

    /// Like [`SimFs::fail_after`], counting only syncs of files and of the names, though a
    /// failure from then on fails every later operation.
    pub(crate) fn fail_sync_after(&self, syncs: u64, failure: Failure) {
        lock(&self.state).fault = Some(Fault {
            remaining: syncs,
            failure,
            inode: None,
            syncs_only: true,
            failing: false,
        });
    }

    /// Like [`SimFs::fail_file_after`], counting only syncs of the file named `name`.
    pub(crate) fn fail_file_sync_after(
        &self,
        name: &str,
        syncs: u64,
        failure: Failure,
    ) -> io::Result<()> {
        let mut state = lock(&self.state);
        let inode = *state.live_names.get(name).ok_or_else(|| not_found(name))?;
        state.fault = Some(Fault {
            remaining: syncs,
            failure,
            inode: Some(inode),
            syncs_only: true,
            failing: false,
        });
        Ok(())
    }

    /// Makes writes that grow the files past `capacity` bytes in all fail as a full device, or
    /// lifts the limit with `None`.
    pub(crate) fn set_capacity(&self, capacity: Option<u64>) {
        lock(&self.state).capacity = capacity;
    }

    /// The bytes every named file holds.
    pub(crate) fn used(&self) -> u64 {
        let state = lock(&self.state);
        state
            .live_names
            .values()
            .map(|inode| state.inodes[*inode].live.len() as u64)
            .sum()
    }

    pub(crate) fn stop_failing(&self) {
        lock(&self.state).fault = None;
    }

    /// The number of operations called so far, failed ones included.
    pub(crate) fn operations(&self) -> u64 {
        lock(&self.state).operations
    }

    /// A separate file system in the state of this one, its unsynced writes and armed
    /// failure included.
    pub(crate) fn fork(&self) -> SimFs {
        SimFs {
            state: Arc::new(Mutex::new(lock(&self.state).clone())),
        }
    }

    /// The file system a power loss now could leave, with every write in it synced and no
    /// failure armed. The old file system and its files keep working.
    pub(crate) fn crash(&self, crash: Crash) -> SimFs {
        let state = lock(&self.state);
        let mut random = SplitMix(match crash {
            Crash::Scramble { seed } | Crash::Reorder { seed } => seed,
            Crash::DropUnsynced | Crash::KeepUnsynced => 0,
        });
        let names = match crash {
            Crash::DropUnsynced => state.durable_names.clone(),
            Crash::KeepUnsynced => state.live_names.clone(),
            Crash::Scramble { .. } | Crash::Reorder { .. } => {
                let kept = random.below(state.pending_names.len() as u64 + 1) as usize;
                let mut names = state.durable_names.clone();
                for change in &state.pending_names[..kept] {
                    State::apply(&mut names, change);
                }
                names
            }
        };
        let mut renumbered = BTreeMap::new();
        let mut inodes = Vec::new();
        for inode in names.values().copied().collect::<BTreeSet<_>>() {
            let stored = &state.inodes[inode];
            let bytes = match crash {
                Crash::DropUnsynced => stored.durable.clone(),
                Crash::KeepUnsynced => stored.live.clone(),
                Crash::Scramble { .. } => scramble(stored, &mut random, Torn::WithGarbage),
                Crash::Reorder { .. } => scramble(stored, &mut random, Torn::OldOrNew),
            };
            renumbered.insert(inode, inodes.len());
            inodes.push(Inode {
                live: bytes.clone(),
                durable: bytes,
                pending: Vec::new(),
                unsettled: Vec::new(),
            });
        }
        let names: BTreeMap<String, usize> = names
            .into_iter()
            .map(|(name, inode)| (name, renumbered[&inode]))
            .collect();
        SimFs {
            state: Arc::new(Mutex::new(State {
                inodes,
                live_names: names.clone(),
                durable_names: names,
                ..State::default()
            })),
        }
    }

    /// Makes every later creation of a side file fail as a read-only directory refuses it.
    pub(crate) fn refuse_side_files(&self) {
        lock(&self.state).refuse_sides = true;
    }

    /// Makes side files unusable as on a file system that shares no mappings: a creation
    /// leaves the file and then fails, and an open of one fails, both as unsupported.
    pub(crate) fn refuse_side_sharing(&self) {
        lock(&self.state).unshared_sides = true;
    }

    fn side(&self, state: &mut State, name: &str, inode: usize) -> SimSideHandle {
        state.handles += 1;
        SimSideHandle {
            side: Arc::clone(state.sides.entry(inode).or_default()),
            handle: state.handles,
            state: Arc::clone(&self.state),
            name: name.to_owned(),
            inode,
        }
    }

    fn file(&self, state: &mut State, inode: usize, size: BlockSize) -> SimFile {
        state.handles += 1;
        let handle = state.handles;
        SimFile {
            state: Arc::clone(&self.state),
            inode,
            size,
            handle,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Torn {
    WithGarbage,
    OldOrNew,
}

fn scramble(inode: &Inode, random: &mut SplitMix, torn: Torn) -> Vec<u8> {
    let mut bytes = inode.durable.clone();
    let mut unsettled: Vec<&DataChange> = inode.unsettled.iter().collect();
    random.shuffle(&mut unsettled);
    let mut pending: Vec<&DataChange> = inode.pending.iter().collect();
    random.shuffle(&mut pending);
    for change in unsettled.into_iter().chain(pending) {
        match change {
            DataChange::SetLength(length) => {
                if random.chance(2) {
                    bytes.resize(*length, 0);
                }
            }
            DataChange::Write { start, data } => {
                let end = start + data.len();
                match random.below(3) {
                    0 => {}
                    1 => {
                        grow(&mut bytes, end);
                        bytes[*start..end].copy_from_slice(data);
                    }
                    _ => {
                        grow(&mut bytes, end);
                        tear(&mut bytes[*start..end], data, random, torn);
                        if torn == Torn::WithGarbage {
                            garble_neighbours(&mut bytes, *start..end, random);
                        }
                    }
                }
            }
        }
    }
    if random.chance(4) {
        let durable = &inode.durable;
        bytes.truncate(durable.len());
        bytes.extend_from_slice(&durable[bytes.len()..]);
    }
    bytes
}

fn garble_neighbours(bytes: &mut [u8], written: Range<usize>, random: &mut SplitMix) {
    let first = written.start / DEVICE_UNIT * DEVICE_UNIT;
    let last = written.end.div_ceil(DEVICE_UNIT) * DEVICE_UNIT;
    for sector in (first..last.min(bytes.len())).step_by(SECTOR_SIZE) {
        if !written.contains(&sector) && random.chance(3) {
            bytes[sector..sector + SECTOR_SIZE].fill_with(|| random.next() as u8);
        }
    }
}

fn grow(bytes: &mut Vec<u8>, end: usize) {
    if bytes.len() < end {
        bytes.resize(end, 0);
    }
}

fn tear(block: &mut [u8], new: &[u8], random: &mut SplitMix, torn: Torn) {
    let (sectors, _) = block.as_chunks_mut::<SECTOR_SIZE>();
    let (new_sectors, _) = new.as_chunks::<SECTOR_SIZE>();
    let outcomes = match torn {
        Torn::WithGarbage => 3,
        Torn::OldOrNew => 2,
    };
    for (sector, new) in sectors.iter_mut().zip(new_sectors) {
        match random.below(outcomes) {
            0 => {}
            1 => *sector = *new,
            _ => sector.fill_with(|| random.next() as u8),
        }
    }
}

#[derive(Debug)]
struct SplitMix(u64);

impl SplitMix {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut mixed = self.0;
        mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        mixed ^ (mixed >> 31)
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }

    fn chance(&mut self, one_in: u64) -> bool {
        self.below(one_in) == 0
    }

    fn shuffle<T>(&mut self, items: &mut [T]) {
        for last in (1..items.len()).rev() {
            items.swap(last, self.below(last as u64 + 1) as usize);
        }
    }
}

impl FileSystem for SimFs {
    type File = SimFile;

    fn create(&self, name: &str, size: BlockSize) -> io::Result<SimFile> {
        let mut state = lock(&self.state);
        state.operate(None, false)?;
        if state.live_names.contains_key(name) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("a file named {name} exists"),
            ));
        }
        let inode = state.inodes.len();
        state.inodes.push(Inode::default());
        state.change_names(NameChange::Create {
            name: name.to_owned(),
            inode,
        });
        Ok(self.file(&mut state, inode, size))
    }

    fn open(&self, name: &str, size: BlockSize) -> io::Result<SimFile> {
        let mut state = lock(&self.state);
        state.operate(None, false)?;
        let inode = *state.live_names.get(name).ok_or_else(|| not_found(name))?;
        Ok(self.file(&mut state, inode, size))
    }

    fn open_side(&self, name: &str) -> io::Result<Box<dyn SideFile>> {
        let mut state = lock(&self.state);
        state.operate(None, false)?;
        let inode = *state.live_names.get(name).ok_or_else(|| not_found(name))?;
        if state.unshared_sides {
            return Err(unshared());
        }
        Ok(Box::new(self.side(&mut state, name, inode)))
    }

    fn create_side(&self, name: &str) -> io::Result<Box<dyn SideFile>> {
        if lock(&self.state).refuse_sides {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "the simulated directory refuses side files",
            ));
        }
        let file = self.create(
            name,
            BlockSize::new(SECTOR_SIZE).expect("a sector is a block"),
        )?;
        let mut state = lock(&self.state);
        if state.unshared_sides {
            return Err(unshared());
        }
        Ok(Box::new(self.side(&mut state, name, file.inode)))
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        let mut state = lock(&self.state);
        state.operate(None, false)?;
        if !state.live_names.contains_key(from) {
            return Err(not_found(from));
        }
        state.change_names(NameChange::Rename {
            from: from.to_owned(),
            to: to.to_owned(),
        });
        Ok(())
    }

    fn remove(&self, name: &str) -> io::Result<()> {
        let mut state = lock(&self.state);
        state.operate(None, false)?;
        if !state.live_names.contains_key(name) {
            return Err(not_found(name));
        }
        state.change_names(NameChange::Remove {
            name: name.to_owned(),
        });
        Ok(())
    }

    fn names(&self) -> io::Result<Vec<String>> {
        let mut state = lock(&self.state);
        state.operate(None, false)?;
        Ok(state.live_names.keys().cloned().collect())
    }

    fn sync_names(&self) -> io::Result<()> {
        let mut state = lock(&self.state);
        if state.names_sync_failed {
            return Err(injected());
        }
        if let Err(error) = state.operate(None, true) {
            state.names_sync_failed = true;
            return Err(error);
        }
        state.durable_names = state.live_names.clone();
        state.pending_names.clear();
        Ok(())
    }
}

impl Drop for SimFile {
    fn drop(&mut self) {
        let mut state = lock(&self.state);
        let released = match state.locks.get_mut(&self.inode) {
            Some(Held::Exclusive(holder)) => *holder == self.handle,
            Some(Held::Shared(holders)) => {
                holders.remove(&self.handle);
                holders.is_empty()
            }
            None => false,
        };
        if released {
            state.locks.remove(&self.inode);
        }
        if let Some(holders) = state.sessions.get_mut(&self.inode) {
            holders.remove(&self.handle);
            if holders.is_empty() {
                state.sessions.remove(&self.inode);
            }
        }
    }
}

impl BlockFile for SimFile {
    fn read(&self, first: u64, buffer: &mut [u8]) -> io::Result<()> {
        let mut state = lock(&self.state);
        state.readable_by(self.inode, self.handle)?;
        state.operate(Some(self.inode), false)?;
        read_blocks(&state.inode(self.inode).live, self.size, first, buffer)
    }

    fn write(&self, first: u64, buffer: &[u8]) -> io::Result<()> {
        let mut state = lock(&self.state);
        state.writable_by(self.inode, self.handle)?;
        state.operate(Some(self.inode), false)?;
        let start = byte_index(self.size.offset(first)?)?;
        state.fits(self.inode, start + buffer.len())?;
        let inode = state.inode(self.inode);
        write_blocks(&mut inode.live, self.size, first, buffer)?;
        let blocks = buffer.chunks(self.size.bytes()).enumerate();
        inode
            .pending
            .extend(blocks.map(|(index, data)| DataChange::Write {
                start: start + index * data.len(),
                data: data.to_vec(),
            }));
        Ok(())
    }

    fn block_count(&self) -> io::Result<u64> {
        let mut state = lock(&self.state);
        state.operate(Some(self.inode), false)?;
        Ok((state.inode(self.inode).live.len() / self.size.bytes()) as u64)
    }

    fn is_empty(&self) -> io::Result<bool> {
        let mut state = lock(&self.state);
        state.operate(Some(self.inode), false)?;
        Ok(state.inode(self.inode).live.is_empty())
    }

    fn lock(&self) -> io::Result<()> {
        let mut state = lock(&self.state);
        let free = match state.locks.get(&self.inode) {
            None => true,
            Some(Held::Exclusive(holder)) => *holder == self.handle,
            Some(Held::Shared(holders)) => holders.iter().all(|holder| *holder == self.handle),
        };
        let in_session = state
            .sessions
            .get(&self.inode)
            .is_some_and(|holders| holders.iter().any(|holder| *holder != self.handle));
        if !free || in_session {
            return Err(held_elsewhere());
        }
        state.locks.insert(self.inode, Held::Exclusive(self.handle));
        Ok(())
    }

    fn lock_session(&self) -> io::Result<()> {
        let mut state = lock(&self.state);
        match state.locks.get(&self.inode) {
            Some(Held::Exclusive(holder)) if *holder != self.handle => {
                return Err(held_elsewhere());
            }
            Some(Held::Exclusive(_)) => {
                state.locks.remove(&self.inode);
            }
            _ => {}
        }
        state
            .sessions
            .entry(self.inode)
            .or_default()
            .insert(self.handle);
        Ok(())
    }

    fn identity(&self) -> io::Result<(u64, u128)> {
        Ok((0, self.inode as u128))
    }

    fn unlock(&self) -> io::Result<()> {
        let mut state = lock(&self.state);
        if matches!(state.locks.get(&self.inode), Some(Held::Exclusive(holder)) if *holder == self.handle)
        {
            state.locks.remove(&self.inode);
        }
        Ok(())
    }

    fn lock_shared(&self) -> io::Result<()> {
        let mut state = lock(&self.state);
        match state.locks.get_mut(&self.inode) {
            Some(Held::Exclusive(holder)) if *holder != self.handle => Err(held_elsewhere()),
            Some(Held::Shared(holders)) => {
                holders.insert(self.handle);
                Ok(())
            }
            _ => {
                state
                    .locks
                    .insert(self.inode, Held::Shared(BTreeSet::from([self.handle])));
                Ok(())
            }
        }
    }

    fn set_block_count(&self, count: u64) -> io::Result<()> {
        let mut state = lock(&self.state);
        state.writable_by(self.inode, self.handle)?;
        state.operate(Some(self.inode), false)?;
        let length = byte_index(self.size.offset(count)?)?;
        state.fits(self.inode, length)?;
        let inode = state.inode(self.inode);
        resize_blocks(&mut inode.live, self.size, count)?;
        let length = inode.live.len();
        inode.pending.push(DataChange::SetLength(length));
        Ok(())
    }

    fn sync(&self) -> io::Result<()> {
        let mut state = lock(&self.state);
        let failed = state.operate(Some(self.inode), true);
        let inode = state.inode(self.inode);
        let pending = mem::take(&mut inode.pending);
        if let Err(error) = failed {
            let writes = pending
                .into_iter()
                .filter(|change| matches!(change, DataChange::Write { .. }));
            inode.unsettled.extend(writes);
            return Err(error);
        }
        for change in pending {
            match change {
                DataChange::SetLength(length) => {
                    inode.durable.resize(length, 0);
                    inode.unsettled.retain(|old| match old {
                        DataChange::Write { start, .. } => *start < length,
                        DataChange::SetLength(_) => true,
                    });
                }
                DataChange::Write { start, data } => {
                    let end = start + data.len();
                    grow(&mut inode.durable, end);
                    inode.durable[start..end].copy_from_slice(&data);
                    inode.unsettled.retain(|old| match old {
                        DataChange::Write { start: old, data } => {
                            old + data.len() <= start || *old >= end
                        }
                        DataChange::SetLength(_) => true,
                    });
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "sim_fs.test.rs"]
mod tests;

// ast-grep-ignore
// Lost at a power loss, as the side file is never synced; locks belong to handles, as OFD locks.
#[derive(Debug)]
struct SimSide {
    words: Box<[AtomicU64]>,
    locks: Mutex<BTreeMap<SideLock, SideHeld>>,
    changed: Condvar,
}

impl Default for SimSide {
    fn default() -> Self {
        Self {
            words: (0..WORDS).map(|_| AtomicU64::new(0)).collect(),
            locks: Mutex::new(BTreeMap::new()),
            changed: Condvar::new(),
        }
    }
}

#[derive(Debug)]
enum SideHeld {
    Exclusive(u64),
    Shared(BTreeSet<u64>),
}

#[derive(Debug)]
struct SimSideHandle {
    side: Arc<SimSide>,
    handle: u64,
    state: Arc<Mutex<State>>,
    name: String,
    inode: usize,
}

fn unshared() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "the simulated file system shares no mappings",
    )
}

#[derive(Debug)]
struct SimWords(Arc<SimSide>);

impl SideWords for SimWords {
    fn word(&self, index: usize) -> &AtomicU64 {
        &self.0.words[index]
    }
}

impl SimSideHandle {
    fn take(&self, locks: &mut BTreeMap<SideLock, SideHeld>, at: SideLock, kind: Lock) -> bool {
        let me = self.handle;
        let granted = match (locks.get(&at), kind) {
            (None, Lock::Exclusive) => SideHeld::Exclusive(me),
            (None, Lock::Shared) => SideHeld::Shared(BTreeSet::from([me])),
            (Some(SideHeld::Exclusive(holder)), _) if *holder != me => return false,
            (Some(SideHeld::Exclusive(_)), Lock::Exclusive) => SideHeld::Exclusive(me),
            (Some(SideHeld::Exclusive(_)), Lock::Shared) => SideHeld::Shared(BTreeSet::from([me])),
            (Some(SideHeld::Shared(holders)), Lock::Exclusive) => {
                if holders.iter().any(|holder| *holder != me) {
                    return false;
                }
                SideHeld::Exclusive(me)
            }
            (Some(SideHeld::Shared(holders)), Lock::Shared) => {
                let mut holders = holders.clone();
                holders.insert(me);
                SideHeld::Shared(holders)
            }
        };
        locks.insert(at, granted);
        true
    }

    fn release(&self, locks: &mut BTreeMap<SideLock, SideHeld>, at: SideLock) {
        let empty = match locks.get_mut(&at) {
            Some(SideHeld::Exclusive(holder)) => *holder == self.handle,
            Some(SideHeld::Shared(holders)) => {
                holders.remove(&self.handle);
                holders.is_empty()
            }
            None => false,
        };
        if empty {
            locks.remove(&at);
        }
        self.side.changed.notify_all();
    }
}

impl SideFile for SimSideHandle {
    fn lock(&self, at: SideLock, kind: Lock) -> io::Result<()> {
        let mut locks = self
            .side
            .locks
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        while !self.take(&mut locks, at, kind) {
            locks = self
                .side
                .changed
                .wait(locks)
                .unwrap_or_else(PoisonError::into_inner);
        }
        Ok(())
    }

    fn try_lock(&self, at: SideLock, kind: Lock) -> io::Result<bool> {
        let mut locks = self
            .side
            .locks
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        Ok(self.take(&mut locks, at, kind))
    }

    fn unlock(&self, at: SideLock) -> io::Result<()> {
        let mut locks = self
            .side
            .locks
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        self.release(&mut locks, at);
        Ok(())
    }

    fn downgrade(&self, at: SideLock) -> io::Result<()> {
        self.try_lock(at, Lock::Shared).map(|_| ())
    }

    fn try_upgrade(&self, at: SideLock) -> io::Result<bool> {
        self.try_lock(at, Lock::Exclusive)
    }

    fn is_named(&self) -> io::Result<bool> {
        Ok(lock(&self.state).live_names.get(&self.name) == Some(&self.inode))
    }

    fn unlink(&self) -> io::Result<()> {
        let mut state = lock(&self.state);
        if state.live_names.get(&self.name) == Some(&self.inode) {
            state.change_names(NameChange::Remove {
                name: self.name.clone(),
            });
        }
        Ok(())
    }

    fn map(&self, _grow: bool) -> io::Result<Box<dyn SideWords>> {
        Ok(Box::new(SimWords(Arc::clone(&self.side))))
    }
}

impl Drop for SimSideHandle {
    fn drop(&mut self) {
        let mut locks = self
            .side
            .locks
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let held: Vec<SideLock> = locks.keys().copied().collect();
        for at in held {
            self.release(&mut locks, at);
        }
    }
}
