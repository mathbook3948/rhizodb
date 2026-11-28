use std::io;
use std::path::Path;
use std::sync::Arc;

use crate::error::ErrorKind;
use crate::storage::ReadTransaction;
use crate::storage::StorageError;
use crate::storage::copy_tables;

use super::block_file::BlockFile;
use super::block_file::BlockSize;
use super::block_file::FileSystem;
use super::error::EngineError;
use super::mem_fs::MemFile;
use super::mem_fs::MemFs;
use super::os_fs::OsFile;
use super::os_fs::OsFs;
use super::session::SideFile;
use super::snapshot::install;
use super::store::Engine;
use super::store::EngineOptions;
use super::store::staging_name;
use super::stray_log::Place;
use super::stray_log::adopt;
use super::wal::segment_files;
use super::wal::segment_name;

/// The file system a database lives on: a directory of the operating system's, or memory.
#[derive(Debug)]
pub(crate) enum AnyFs {
    Os(OsFs),
    Memory(MemFs),
    #[cfg(test)]
    Sim(super::sim_fs::SimFs),
}

#[derive(Debug)]
pub(crate) enum AnyFile {
    Os(OsFile),
    Memory(MemFile),
    #[cfg(test)]
    Sim(super::sim_fs::SimFile),
}

impl FileSystem for AnyFs {
    type File = AnyFile;

    fn create(&self, name: &str, size: BlockSize) -> io::Result<AnyFile> {
        match self {
            AnyFs::Os(fs) => fs.create(name, size).map(AnyFile::Os),
            AnyFs::Memory(fs) => fs.create(name, size).map(AnyFile::Memory),
            #[cfg(test)]
            AnyFs::Sim(fs) => fs.create(name, size).map(AnyFile::Sim),
        }
    }

    fn open(&self, name: &str, size: BlockSize) -> io::Result<AnyFile> {
        match self {
            AnyFs::Os(fs) => fs.open(name, size).map(AnyFile::Os),
            AnyFs::Memory(fs) => fs.open(name, size).map(AnyFile::Memory),
            #[cfg(test)]
            AnyFs::Sim(fs) => fs.open(name, size).map(AnyFile::Sim),
        }
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        match self {
            AnyFs::Os(fs) => fs.rename(from, to),
            AnyFs::Memory(fs) => fs.rename(from, to),
            #[cfg(test)]
            AnyFs::Sim(fs) => fs.rename(from, to),
        }
    }

    fn remove(&self, name: &str) -> io::Result<()> {
        match self {
            AnyFs::Os(fs) => fs.remove(name),
            AnyFs::Memory(fs) => fs.remove(name),
            #[cfg(test)]
            AnyFs::Sim(fs) => fs.remove(name),
        }
    }

    fn names(&self) -> io::Result<Vec<String>> {
        match self {
            AnyFs::Os(fs) => fs.names(),
            AnyFs::Memory(fs) => fs.names(),
            #[cfg(test)]
            AnyFs::Sim(fs) => fs.names(),
        }
    }

    fn sync_names(&self) -> io::Result<()> {
        match self {
            AnyFs::Os(fs) => fs.sync_names(),
            AnyFs::Memory(fs) => fs.sync_names(),
            #[cfg(test)]
            AnyFs::Sim(fs) => fs.sync_names(),
        }
    }

    fn open_side(&self, name: &str) -> io::Result<Box<dyn SideFile>> {
        match self {
            AnyFs::Os(fs) => fs.open_side(name),
            AnyFs::Memory(fs) => fs.open_side(name),
            #[cfg(test)]
            AnyFs::Sim(fs) => fs.open_side(name),
        }
    }

    fn remove_side(&self, name: &str) -> io::Result<()> {
        match self {
            AnyFs::Os(fs) => fs.remove_side(name),
            AnyFs::Memory(fs) => fs.remove_side(name),
            #[cfg(test)]
            AnyFs::Sim(fs) => fs.remove_side(name),
        }
    }

    fn create_side(&self, name: &str) -> io::Result<Box<dyn SideFile>> {
        match self {
            AnyFs::Os(fs) => fs.create_side(name),
            AnyFs::Memory(fs) => fs.create_side(name),
            #[cfg(test)]
            AnyFs::Sim(fs) => fs.create_side(name),
        }
    }
}

impl BlockFile for AnyFile {
    fn read(&self, first: u64, buffer: &mut [u8]) -> io::Result<()> {
        match self {
            AnyFile::Os(file) => file.read(first, buffer),
            AnyFile::Memory(file) => file.read(first, buffer),
            #[cfg(test)]
            AnyFile::Sim(file) => file.read(first, buffer),
        }
    }

    fn write(&self, first: u64, buffer: &[u8]) -> io::Result<()> {
        match self {
            AnyFile::Os(file) => file.write(first, buffer),
            AnyFile::Memory(file) => file.write(first, buffer),
            #[cfg(test)]
            AnyFile::Sim(file) => file.write(first, buffer),
        }
    }

    fn block_count(&self) -> io::Result<u64> {
        match self {
            AnyFile::Os(file) => file.block_count(),
            AnyFile::Memory(file) => file.block_count(),
            #[cfg(test)]
            AnyFile::Sim(file) => file.block_count(),
        }
    }

    fn is_empty(&self) -> io::Result<bool> {
        match self {
            AnyFile::Os(file) => file.is_empty(),
            AnyFile::Memory(file) => file.is_empty(),
            #[cfg(test)]
            AnyFile::Sim(file) => file.is_empty(),
        }
    }

    fn lock(&self) -> io::Result<()> {
        match self {
            AnyFile::Os(file) => file.lock(),
            AnyFile::Memory(file) => file.lock(),
            #[cfg(test)]
            AnyFile::Sim(file) => file.lock(),
        }
    }

    fn lock_shared(&self) -> io::Result<()> {
        match self {
            AnyFile::Os(file) => file.lock_shared(),
            AnyFile::Memory(file) => file.lock_shared(),
            #[cfg(test)]
            AnyFile::Sim(file) => file.lock_shared(),
        }
    }

    fn lock_session(&self, first: bool) -> io::Result<()> {
        match self {
            AnyFile::Os(file) => file.lock_session(first),
            AnyFile::Memory(file) => file.lock_session(first),
            #[cfg(test)]
            AnyFile::Sim(file) => file.lock_session(first),
        }
    }

    fn unlock(&self) -> io::Result<()> {
        match self {
            AnyFile::Os(file) => file.unlock(),
            AnyFile::Memory(file) => file.unlock(),
            #[cfg(test)]
            AnyFile::Sim(file) => file.unlock(),
        }
    }

    fn held_elsewhere(&self) -> io::Result<bool> {
        match self {
            AnyFile::Os(file) => file.held_elsewhere(),
            AnyFile::Memory(file) => file.held_elsewhere(),
            #[cfg(test)]
            AnyFile::Sim(file) => file.held_elsewhere(),
        }
    }

    fn identity(&self) -> io::Result<(u64, u128)> {
        match self {
            AnyFile::Os(file) => file.identity(),
            AnyFile::Memory(file) => file.identity(),
            #[cfg(test)]
            AnyFile::Sim(file) => file.identity(),
        }
    }

    fn set_block_count(&self, count: u64) -> io::Result<()> {
        match self {
            AnyFile::Os(file) => file.set_block_count(count),
            AnyFile::Memory(file) => file.set_block_count(count),
            #[cfg(test)]
            AnyFile::Sim(file) => file.set_block_count(count),
        }
    }

    fn sync(&self) -> io::Result<()> {
        match self {
            AnyFile::Os(file) => file.sync(),
            AnyFile::Memory(file) => file.sync(),
            #[cfg(test)]
            AnyFile::Sim(file) => file.sync(),
        }
    }
}

const MEMORY_NAME: &str = "memory";

const MOST_SYMLINKS: usize = 40;

#[derive(Debug)]
struct Resolved {
    real: Place,
    given: Place,
}

impl Resolved {
    fn of(path: &Path) -> io::Result<Self> {
        let given = Place::of(path)?;
        let mut at = path.to_path_buf();
        for _ in 0..MOST_SYMLINKS {
            match std::fs::symlink_metadata(&at) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    let target = std::fs::read_link(&at)?;
                    at = at.parent().unwrap_or(Path::new("")).join(target);
                }
                Ok(_) => {
                    refuse_hard_links(&at)?;
                    return Ok(Self {
                        real: Place::of(&at)?,
                        given,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    return Ok(Self {
                        real: Place::of(&at)?,
                        given,
                    });
                }
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "the database path {} passes through more than {MOST_SYMLINKS} symlinks",
                path.display()
            ),
        ))
    }

    fn fs(&self) -> Arc<AnyFs> {
        Arc::new(AnyFs::Os(OsFs::new(&self.real.directory)))
    }

    fn stray_segments(&self) -> io::Result<Vec<u64>> {
        if self.given == self.real {
            return Ok(Vec::new());
        }
        self.given.segments()
    }
}

#[cfg(unix)]
fn refuse_hard_links(real: &Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let links = std::fs::metadata(real)?.nlink();
    if links > 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "the database file {} has {links} hard links, and the log segments beside it \
                 are named after one of them, so copy the file instead of linking it",
                real.display()
            ),
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn refuse_hard_links(_real: &Path) -> io::Result<()> {
    Ok(())
}

/// Opens the database file at `path` with the engine, creating it when there is none. Log
/// segments that an older release named after a symlink to the file move beside the file once
/// the open finds the log missing, or finds that an earlier move stopped part way.
pub(crate) fn open_path(path: &Path, options: EngineOptions) -> Result<Engine<AnyFs>, EngineError> {
    let resolved = Resolved::of(path)?;
    let open = || Engine::open(resolved.fs(), &resolved.real.name, options);
    if resolved.stray_segments()?.is_empty() {
        return open();
    }
    if !resolved.real.segments()?.is_empty() {
        adopt(&resolved.real, &resolved.given, options)?;
        return open();
    }
    match open() {
        Err(EngineError::WalMissing) => {
            adopt(&resolved.real, &resolved.given, options)?;
            open()
        }
        opened => opened,
    }
}

/// Opens the database file at `path` as the process of its multi-process session that writes,
/// as [`Engine::open_shared`] does.
pub(crate) fn open_path_shared(
    path: &Path,
    options: EngineOptions,
) -> Result<Engine<AnyFs>, EngineError> {
    let (directory, name) = split(path)?;
    if !directory.join(name).exists() {
        Engine::open(Arc::new(AnyFs::Os(OsFs::new(&directory))), name, options)?.close()?;
    }
    let fs = OsFs::shared(directory, name)?;
    Engine::open_shared(Arc::new(AnyFs::Os(fs)), name, options)
}

/// Opens the existing database file at `path` with the engine read-only, as
/// [`Engine::open_read_only`] does. Fails where [`open_path`] would move log segments.
pub(crate) fn open_path_read_only(
    path: &Path,
    options: EngineOptions,
) -> Result<Engine<AnyFs>, EngineError> {
    let resolved = Resolved::of(path)?;
    const STRAY: EngineError = EngineError::ReadOnly(
        "move the log that an older release left beside a symlink to the file, under the \
         symlink's name, to the file, which one open that writes does",
    );
    let stray = !resolved.stray_segments()?.is_empty();
    if stray && !resolved.real.segments()?.is_empty() {
        return Err(STRAY);
    }
    let fs = OsFs::read_only(&resolved.real.directory, &resolved.real.name)?;
    match Engine::open_read_only(Arc::new(AnyFs::Os(fs)), &resolved.real.name, options) {
        Err(EngineError::WalMissing) if stray => Err(STRAY),
        opened => opened,
    }
}

/// Installs the snapshot `input` streams as the database file at `path`, as
/// [`super::snapshot::install`] does, and opens it.
pub(crate) fn install_path(
    path: &Path,
    input: impl io::Read,
    options: EngineOptions,
    force: bool,
) -> Result<Engine<AnyFs>, EngineError> {
    let resolved = Resolved::of(path)?;
    install(resolved.fs(), &resolved.real.name, input, options, force)
}

/// A database of the engine that lives in memory and is gone when dropped.
pub(crate) fn in_memory(options: EngineOptions) -> Result<Engine<AnyFs>, EngineError> {
    Engine::open(
        Arc::new(AnyFs::Memory(MemFs::default())),
        MEMORY_NAME,
        options,
    )
}

/// Writes the tables `snapshot` sees into a new engine database at `path`, and closes it, which
/// checkpoints its whole log into the file and removes the log's segments. Fails when a segment
/// is left, since the file alone would then miss commits.
pub(crate) fn write_copy(snapshot: &impl ReadTransaction, path: &Path) -> Result<(), StorageError> {
    let resolved = Resolved::of(path).map_err(|error| {
        StorageError::io(
            ErrorKind::Storage,
            format!("cannot back up to {}", path.display()),
            error,
        )
    })?;
    write_copy_on(snapshot, resolved.fs(), &resolved.real.name)
}

/// [`write_copy`] into the database file `name` on `fs`, removing the copy's log segments and
/// staging file when it fails.
pub(crate) fn write_copy_on<F: FileSystem + 'static>(
    snapshot: &impl ReadTransaction,
    fs: Arc<F>,
    name: &str,
) -> Result<(), StorageError> {
    let copied = Engine::open(Arc::clone(&fs), name, EngineOptions::default())
        .map_err(StorageError::from)
        .and_then(|copy| {
            copy_tables(snapshot, &copy)?;
            copy.close().map_err(StorageError::from)
        });
    let left = match segment_files(&*fs, name) {
        Ok(left) => left,
        Err(error) => {
            copied?;
            return Err(StorageError::io(
                ErrorKind::Storage,
                "cannot list the log segments of the backup",
                error,
            ));
        }
    };
    if copied.is_ok() && left.is_empty() {
        return Ok(());
    }
    for sequence in left {
        let _ = fs.remove(&segment_name(name, sequence));
    }
    let _ = fs.remove(&staging_name(name));
    copied?;
    Err(StorageError::new(io::Error::other(
        "closing the copy left a log segment beside it",
    )))
}

#[cfg(test)]
#[path = "any_fs.test.rs"]
mod tests;
