use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::io;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;

use super::mem_fs::page_index;
use super::mem_fs::read_pages;
use super::mem_fs::write_pages;
use super::page_file::FileSystem;
use super::page_file::PAGE_SIZE;
use super::page_file::Page;
use super::page_file::PageFile;

/// The size of the unit a torn write keeps or loses as a whole.
pub(crate) const SECTOR_SIZE: usize = 512;

/// A deterministic simulated file system that tells what was synced from what was only
/// written, so a test can build the files a power loss could leave and inject I/O errors.
#[derive(Clone, Debug, Default)]
pub(crate) struct SimFs {
    state: Arc<Mutex<State>>,
}

/// A file of [`SimFs`]. It keeps working after its name is removed, as on POSIX.
#[derive(Debug)]
pub(crate) struct SimFile {
    state: Arc<Mutex<State>>,
    inode: usize,
}

/// Which of the writes since the last sync a simulated power loss keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Crash {
    /// Only synced pages, page counts, and names survive.
    DropUnsynced,
    /// Every write reached the disk.
    KeepUnsynced,
    /// The seed picks, for each unsynced write, whether it is lost, kept, or torn at sectors
    /// into new, old, and garbage bytes, applies the kept ones in a shuffled order, may lose
    /// an unsynced change of page count, and keeps a prefix of the unsynced name changes.
    Scramble { seed: u64 },
}

/// How long an injected I/O error lasts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Failure {
    /// Only the chosen operation fails.
    Once,
    /// The chosen operation and every later one fail.
    FromThenOn,
}

#[derive(Debug, Default)]
struct State {
    inodes: Vec<Inode>,
    live_names: BTreeMap<String, usize>,
    durable_names: BTreeMap<String, usize>,
    pending_names: Vec<NameChange>,
    names_sync_failed: bool,
    operations: u64,
    fault: Option<Fault>,
}

#[derive(Clone, Debug, Default)]
struct Inode {
    live: Vec<Page>,
    durable: Vec<Page>,
    pending: Vec<DataChange>,
    sync_failed: bool,
}

#[derive(Clone, Debug)]
enum DataChange {
    Write { page: u64, data: Box<Page> },
    SetPageCount(u64),
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

impl State {
    fn operate(&mut self, inode: Option<usize>) -> io::Result<()> {
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
            failing: false,
        });
        Ok(())
    }

    pub(crate) fn stop_failing(&self) {
        lock(&self.state).fault = None;
    }

    /// The number of operations called so far, failed ones included.
    pub(crate) fn operations(&self) -> u64 {
        lock(&self.state).operations
    }

    /// The file system a power loss now could leave, with every write in it synced and no
    /// failure armed. The old file system and its files keep working.
    pub(crate) fn crash(&self, crash: Crash) -> SimFs {
        let state = lock(&self.state);
        let mut random = SplitMix(match crash {
            Crash::Scramble { seed } => seed,
            Crash::DropUnsynced | Crash::KeepUnsynced => 0,
        });
        let names = match crash {
            Crash::DropUnsynced => state.durable_names.clone(),
            Crash::KeepUnsynced => state.live_names.clone(),
            Crash::Scramble { .. } => {
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
            let pages = match crash {
                Crash::DropUnsynced => stored.durable.clone(),
                Crash::KeepUnsynced => stored.live.clone(),
                Crash::Scramble { .. } => scramble(stored, &mut random),
            };
            renumbered.insert(inode, inodes.len());
            inodes.push(Inode {
                live: pages.clone(),
                durable: pages,
                pending: Vec::new(),
                sync_failed: false,
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

    fn file(&self, inode: usize) -> SimFile {
        SimFile {
            state: Arc::clone(&self.state),
            inode,
        }
    }
}

fn scramble(inode: &Inode, random: &mut SplitMix) -> Vec<Page> {
    let mut pages = inode.durable.clone();
    let mut order: Vec<&DataChange> = inode.pending.iter().collect();
    random.shuffle(&mut order);
    for change in order {
        match change {
            DataChange::SetPageCount(count) => {
                if random.chance(2) {
                    pages.resize(*count as usize, [0; PAGE_SIZE]);
                }
            }
            DataChange::Write { page, data } => {
                let index = *page as usize;
                match random.below(3) {
                    0 => {}
                    1 => {
                        grow(&mut pages, index);
                        pages[index] = **data;
                    }
                    _ => {
                        grow(&mut pages, index);
                        tear(&mut pages[index], data, random);
                    }
                }
            }
        }
    }
    if random.chance(4) {
        let durable = &inode.durable;
        pages.truncate(durable.len());
        pages.extend_from_slice(&durable[pages.len()..]);
    }
    pages
}

fn grow(pages: &mut Vec<Page>, index: usize) {
    if pages.len() <= index {
        pages.resize(index + 1, [0; PAGE_SIZE]);
    }
}

fn tear(page: &mut Page, new: &Page, random: &mut SplitMix) {
    let (sectors, _) = page.as_chunks_mut::<SECTOR_SIZE>();
    let (new_sectors, _) = new.as_chunks::<SECTOR_SIZE>();
    for (sector, new) in sectors.iter_mut().zip(new_sectors) {
        match random.below(3) {
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

    fn create(&self, name: &str) -> io::Result<SimFile> {
        let mut state = lock(&self.state);
        state.operate(None)?;
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
        Ok(self.file(inode))
    }

    fn open(&self, name: &str) -> io::Result<SimFile> {
        let mut state = lock(&self.state);
        state.operate(None)?;
        let inode = *state.live_names.get(name).ok_or_else(|| not_found(name))?;
        Ok(self.file(inode))
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        let mut state = lock(&self.state);
        state.operate(None)?;
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
        state.operate(None)?;
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
        state.operate(None)?;
        Ok(state.live_names.keys().cloned().collect())
    }

    fn sync_names(&self) -> io::Result<()> {
        let mut state = lock(&self.state);
        if state.names_sync_failed {
            return Err(injected());
        }
        if let Err(error) = state.operate(None) {
            state.names_sync_failed = true;
            return Err(error);
        }
        state.durable_names = state.live_names.clone();
        state.pending_names.clear();
        Ok(())
    }
}

impl PageFile for SimFile {
    fn read(&self, first: u64, pages: &mut [Page]) -> io::Result<()> {
        let mut state = lock(&self.state);
        state.operate(Some(self.inode))?;
        read_pages(&state.inode(self.inode).live, first, pages)
    }

    fn write(&self, first: u64, pages: &[Page]) -> io::Result<()> {
        let mut state = lock(&self.state);
        state.operate(Some(self.inode))?;
        let inode = state.inode(self.inode);
        write_pages(&mut inode.live, first, pages)?;
        inode.pending.extend(
            pages
                .iter()
                .zip(first..)
                .map(|(data, page)| DataChange::Write {
                    page,
                    data: Box::new(*data),
                }),
        );
        Ok(())
    }

    fn page_count(&self) -> io::Result<u64> {
        let mut state = lock(&self.state);
        state.operate(Some(self.inode))?;
        Ok(state.inode(self.inode).live.len() as u64)
    }

    fn set_page_count(&self, count: u64) -> io::Result<()> {
        let mut state = lock(&self.state);
        state.operate(Some(self.inode))?;
        let inode = state.inode(self.inode);
        inode.live.resize(page_index(count)?, [0; PAGE_SIZE]);
        inode.pending.push(DataChange::SetPageCount(count));
        Ok(())
    }

    fn sync(&self) -> io::Result<()> {
        let mut state = lock(&self.state);
        if state.inode(self.inode).sync_failed {
            return Err(injected());
        }
        if let Err(error) = state.operate(Some(self.inode)) {
            state.inode(self.inode).sync_failed = true;
            return Err(error);
        }
        let inode = state.inode(self.inode);
        inode.durable = inode.live.clone();
        inode.pending.clear();
        Ok(())
    }
}

#[cfg(test)]
#[path = "sim_fs.test.rs"]
mod tests;
