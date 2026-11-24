use std::fs;
use std::fs::File;
use std::fs::TryLockError;
use std::io;
use std::path::PathBuf;

use super::block_file::BlockFile;
use super::block_file::BlockSize;
use super::block_file::FileSystem;

/// The files of one directory of the operating system's file system.
#[derive(Debug)]
pub(crate) struct OsFs {
    directory: PathBuf,
}

impl OsFs {
    pub(crate) fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
        }
    }
}

#[derive(Debug)]
pub(crate) struct OsFile {
    file: File,
    size: BlockSize,
}

fn lock_or_skip_where_unsupported(file: &File) -> io::Result<()> {
    match file.try_lock() {
        Ok(()) => Ok(()),
        Err(TryLockError::WouldBlock) => Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "the database file is already open",
        )),
        Err(TryLockError::Error(error)) if error.kind() == io::ErrorKind::Unsupported => Ok(()),
        Err(TryLockError::Error(error)) => Err(error),
    }
}

impl FileSystem for OsFs {
    type File = OsFile;

    fn create(&self, name: &str, size: BlockSize) -> io::Result<OsFile> {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(self.directory.join(name))?;
        Ok(OsFile { file, size })
    }

    fn open(&self, name: &str, size: BlockSize) -> io::Result<OsFile> {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(self.directory.join(name))?;
        Ok(OsFile { file, size })
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        fs::rename(self.directory.join(from), self.directory.join(to))
    }

    fn remove(&self, name: &str) -> io::Result<()> {
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

impl BlockFile for OsFile {
    fn block_size(&self) -> BlockSize {
        self.size
    }

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
        lock_or_skip_where_unsupported(&self.file)
    }

    fn set_block_count(&self, count: u64) -> io::Result<()> {
        self.file.set_len(self.size.offset(count)?)
    }

    fn sync(&self) -> io::Result<()> {
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
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(windows)]
fn write_all_at(file: &File, mut buffer: &[u8], mut offset: u64) -> io::Result<()> {
    while !buffer.is_empty() {
        match std::os::windows::fs::FileExt::seek_write(file, buffer, offset) {
            Ok(0) => return Err(io::Error::from(io::ErrorKind::WriteZero)),
            Ok(written) => {
                buffer = &buffer[written..];
                offset += written as u64;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}
