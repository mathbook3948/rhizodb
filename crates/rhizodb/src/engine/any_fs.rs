use std::io;
use std::path::Path;
use std::sync::Arc;

use super::block_file::BlockFile;
use super::block_file::BlockSize;
use super::block_file::FileSystem;
use super::error::EngineError;
use super::mem_fs::MemFile;
use super::mem_fs::MemFs;
use super::os_fs::OsFile;
use super::os_fs::OsFs;
use super::store::Engine;
use super::store::EngineOptions;

/// The file system a database lives on: a directory of the operating system's, or memory.
#[derive(Debug)]
pub(crate) enum AnyFs {
    Os(OsFs),
    Memory(MemFs),
}

#[derive(Debug)]
pub(crate) enum AnyFile {
    Os(OsFile),
    Memory(MemFile),
}

impl FileSystem for AnyFs {
    type File = AnyFile;

    fn create(&self, name: &str, size: BlockSize) -> io::Result<AnyFile> {
        match self {
            AnyFs::Os(fs) => fs.create(name, size).map(AnyFile::Os),
            AnyFs::Memory(fs) => fs.create(name, size).map(AnyFile::Memory),
        }
    }

    fn open(&self, name: &str, size: BlockSize) -> io::Result<AnyFile> {
        match self {
            AnyFs::Os(fs) => fs.open(name, size).map(AnyFile::Os),
            AnyFs::Memory(fs) => fs.open(name, size).map(AnyFile::Memory),
        }
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        match self {
            AnyFs::Os(fs) => fs.rename(from, to),
            AnyFs::Memory(fs) => fs.rename(from, to),
        }
    }

    fn remove(&self, name: &str) -> io::Result<()> {
        match self {
            AnyFs::Os(fs) => fs.remove(name),
            AnyFs::Memory(fs) => fs.remove(name),
        }
    }

    fn names(&self) -> io::Result<Vec<String>> {
        match self {
            AnyFs::Os(fs) => fs.names(),
            AnyFs::Memory(fs) => fs.names(),
        }
    }

    fn sync_names(&self) -> io::Result<()> {
        match self {
            AnyFs::Os(fs) => fs.sync_names(),
            AnyFs::Memory(fs) => fs.sync_names(),
        }
    }
}

impl BlockFile for AnyFile {
    fn block_size(&self) -> BlockSize {
        match self {
            AnyFile::Os(file) => file.block_size(),
            AnyFile::Memory(file) => file.block_size(),
        }
    }

    fn read(&self, first: u64, buffer: &mut [u8]) -> io::Result<()> {
        match self {
            AnyFile::Os(file) => file.read(first, buffer),
            AnyFile::Memory(file) => file.read(first, buffer),
        }
    }

    fn write(&self, first: u64, buffer: &[u8]) -> io::Result<()> {
        match self {
            AnyFile::Os(file) => file.write(first, buffer),
            AnyFile::Memory(file) => file.write(first, buffer),
        }
    }

    fn block_count(&self) -> io::Result<u64> {
        match self {
            AnyFile::Os(file) => file.block_count(),
            AnyFile::Memory(file) => file.block_count(),
        }
    }

    fn is_empty(&self) -> io::Result<bool> {
        match self {
            AnyFile::Os(file) => file.is_empty(),
            AnyFile::Memory(file) => file.is_empty(),
        }
    }

    fn lock(&self) -> io::Result<()> {
        match self {
            AnyFile::Os(file) => file.lock(),
            AnyFile::Memory(file) => file.lock(),
        }
    }

    fn set_block_count(&self, count: u64) -> io::Result<()> {
        match self {
            AnyFile::Os(file) => file.set_block_count(count),
            AnyFile::Memory(file) => file.set_block_count(count),
        }
    }

    fn sync(&self) -> io::Result<()> {
        match self {
            AnyFile::Os(file) => file.sync(),
            AnyFile::Memory(file) => file.sync(),
        }
    }
}

const MEMORY_NAME: &str = "memory";

/// Opens the database file at `path` with the engine, creating it when there is none.
pub(crate) fn open_path(path: &Path, options: EngineOptions) -> Result<Engine<AnyFs>, EngineError> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| EngineError::Unsupported(format!("the database path {}", path.display())))?;
    let directory = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => Path::new(".").to_path_buf(),
    };
    Engine::open(Arc::new(AnyFs::Os(OsFs::new(directory))), name, options)
}

/// A database of the engine that lives in memory and is gone when dropped.
pub(crate) fn in_memory(options: EngineOptions) -> Result<Engine<AnyFs>, EngineError> {
    Engine::open(
        Arc::new(AnyFs::Memory(MemFs::default())),
        MEMORY_NAME,
        options,
    )
}
