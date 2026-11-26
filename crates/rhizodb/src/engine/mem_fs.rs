use std::collections::BTreeMap;
use std::io;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;

use super::block_file::BlockFile;
use super::block_file::BlockSize;
use super::block_file::FileSystem;

type Bytes = Arc<Mutex<Vec<u8>>>;

/// Files held in memory, for databases that are never written to disk.
#[derive(Debug, Default)]
pub(crate) struct MemFs {
    files: Mutex<BTreeMap<String, Bytes>>,
}

/// A file of [`MemFs`]. Every handle of one file shares its bytes, which outlive its name.
#[derive(Debug)]
pub(crate) struct MemFile {
    size: BlockSize,
    bytes: Bytes,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn not_found(name: &str) -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, format!("no file named {name}"))
}

pub(crate) fn byte_index(offset: u64) -> io::Result<usize> {
    usize::try_from(offset)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "offset out of range"))
}

pub(crate) fn read_blocks(
    stored: &[u8],
    size: BlockSize,
    first: u64,
    buffer: &mut [u8],
) -> io::Result<()> {
    size.require_whole_blocks(buffer.len())?;
    let start = byte_index(size.offset(first)?)?;
    let source = start
        .checked_add(buffer.len())
        .and_then(|end| stored.get(start..end))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "read past the end of the file",
            )
        })?;
    buffer.copy_from_slice(source);
    Ok(())
}

pub(crate) fn write_blocks(
    stored: &mut Vec<u8>,
    size: BlockSize,
    first: u64,
    buffer: &[u8],
) -> io::Result<()> {
    size.require_whole_blocks(buffer.len())?;
    let start = byte_index(size.offset(first)?)?;
    let end = start
        .checked_add(buffer.len())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "offset out of range"))?;
    if stored.len() < end {
        stored.resize(end, 0);
    }
    stored[start..end].copy_from_slice(buffer);
    Ok(())
}

pub(crate) fn resize_blocks(stored: &mut Vec<u8>, size: BlockSize, count: u64) -> io::Result<()> {
    stored.resize(byte_index(size.offset(count)?)?, 0);
    Ok(())
}

impl FileSystem for MemFs {
    type File = MemFile;

    fn create(&self, name: &str, size: BlockSize) -> io::Result<MemFile> {
        let mut files = lock(&self.files);
        if files.contains_key(name) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("a file named {name} exists"),
            ));
        }
        let bytes = Bytes::default();
        files.insert(name.to_owned(), Arc::clone(&bytes));
        Ok(MemFile { size, bytes })
    }

    fn open(&self, name: &str, size: BlockSize) -> io::Result<MemFile> {
        let bytes = lock(&self.files)
            .get(name)
            .cloned()
            .ok_or_else(|| not_found(name))?;
        Ok(MemFile { size, bytes })
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        let mut files = lock(&self.files);
        let file = files.remove(from).ok_or_else(|| not_found(from))?;
        files.insert(to.to_owned(), file);
        Ok(())
    }

    fn remove(&self, name: &str) -> io::Result<()> {
        lock(&self.files)
            .remove(name)
            .map(drop)
            .ok_or_else(|| not_found(name))
    }

    fn names(&self) -> io::Result<Vec<String>> {
        Ok(lock(&self.files).keys().cloned().collect())
    }

    fn sync_names(&self) -> io::Result<()> {
        Ok(())
    }
}

impl BlockFile for MemFile {
    fn read(&self, first: u64, buffer: &mut [u8]) -> io::Result<()> {
        read_blocks(&lock(&self.bytes), self.size, first, buffer)
    }

    fn write(&self, first: u64, buffer: &[u8]) -> io::Result<()> {
        write_blocks(&mut lock(&self.bytes), self.size, first, buffer)
    }

    fn block_count(&self) -> io::Result<u64> {
        Ok((lock(&self.bytes).len() / self.size.bytes()) as u64)
    }

    fn is_empty(&self) -> io::Result<bool> {
        Ok(lock(&self.bytes).is_empty())
    }

    fn lock(&self) -> io::Result<()> {
        Ok(())
    }

    fn set_block_count(&self, count: u64) -> io::Result<()> {
        resize_blocks(&mut lock(&self.bytes), self.size, count)
    }

    fn sync(&self) -> io::Result<()> {
        Ok(())
    }
}
