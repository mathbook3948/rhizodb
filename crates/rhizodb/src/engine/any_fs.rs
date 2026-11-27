use std::io;
use std::path::Path;
use std::path::PathBuf;
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
use super::snapshot::install;
use super::store::Engine;
use super::store::EngineOptions;
use super::store::staging_name;
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

fn split(path: &Path) -> Result<(PathBuf, &str), EngineError> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            EngineError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "the database path {} names no file in UTF-8",
                    path.display()
                ),
            ))
        })?;
    let directory = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => Path::new(".").to_path_buf(),
    };
    Ok((directory, name))
}

/// Opens the database file at `path` with the engine, creating it when there is none.
pub(crate) fn open_path(path: &Path, options: EngineOptions) -> Result<Engine<AnyFs>, EngineError> {
    let (directory, name) = split(path)?;
    Engine::open(Arc::new(AnyFs::Os(OsFs::new(directory))), name, options)
}

/// Installs the snapshot `input` streams as the database file at `path`, as
/// [`super::snapshot::install`] does, and opens it.
pub(crate) fn install_path(
    path: &Path,
    input: impl io::Read,
    options: EngineOptions,
    force: bool,
) -> Result<Engine<AnyFs>, EngineError> {
    let (directory, name) = split(path)?;
    install(
        Arc::new(AnyFs::Os(OsFs::new(directory))),
        name,
        input,
        options,
        force,
    )
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
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            StorageError::new(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("the backup path {} names no file in UTF-8", path.display()),
            ))
        })?;
    let directory = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => Path::new(".").to_path_buf(),
    };
    write_copy_on(snapshot, Arc::new(AnyFs::Os(OsFs::new(directory))), name)
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
