use std::collections::BTreeMap;
use std::io;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;

use super::page_file::FileSystem;
use super::page_file::PAGE_SIZE;
use super::page_file::Page;
use super::page_file::PageFile;

/// Files held in memory, for databases that are never written to disk.
#[derive(Debug, Default)]
pub(crate) struct MemFs {
    files: Mutex<BTreeMap<String, MemFile>>,
}

/// A file of [`MemFs`]. Its clones share the same pages, which outlive the file's name.
#[derive(Clone, Debug, Default)]
pub(crate) struct MemFile {
    pages: Arc<Mutex<Vec<Page>>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn not_found(name: &str) -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, format!("no file named {name}"))
}

pub(crate) fn page_index(page: u64) -> io::Result<usize> {
    usize::try_from(page)
        .ok()
        .filter(|index| index.checked_mul(PAGE_SIZE).is_some())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "page number out of range"))
}

pub(crate) fn read_pages(stored: &[Page], first: u64, pages: &mut [Page]) -> io::Result<()> {
    let start = page_index(first)?;
    let source = start
        .checked_add(pages.len())
        .and_then(|end| stored.get(start..end))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "read past the end of the file",
            )
        })?;
    pages.copy_from_slice(source);
    Ok(())
}

pub(crate) fn write_pages(stored: &mut Vec<Page>, first: u64, pages: &[Page]) -> io::Result<()> {
    let start = page_index(first)?;
    let end = page_index(first.saturating_add(pages.len() as u64))?;
    if stored.len() < end {
        stored.resize(end, [0; PAGE_SIZE]);
    }
    stored[start..end].copy_from_slice(pages);
    Ok(())
}

impl FileSystem for MemFs {
    type File = MemFile;

    fn create(&self, name: &str) -> io::Result<MemFile> {
        let mut files = lock(&self.files);
        if files.contains_key(name) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("a file named {name} exists"),
            ));
        }
        let file = MemFile::default();
        files.insert(name.to_owned(), file.clone());
        Ok(file)
    }

    fn open(&self, name: &str) -> io::Result<MemFile> {
        lock(&self.files)
            .get(name)
            .cloned()
            .ok_or_else(|| not_found(name))
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

impl PageFile for MemFile {
    fn read(&self, first: u64, pages: &mut [Page]) -> io::Result<()> {
        read_pages(&lock(&self.pages), first, pages)
    }

    fn write(&self, first: u64, pages: &[Page]) -> io::Result<()> {
        write_pages(&mut lock(&self.pages), first, pages)
    }

    fn page_count(&self) -> io::Result<u64> {
        Ok(lock(&self.pages).len() as u64)
    }

    fn set_page_count(&self, count: u64) -> io::Result<()> {
        lock(&self.pages).resize(page_index(count)?, [0; PAGE_SIZE]);
        Ok(())
    }

    fn sync(&self) -> io::Result<()> {
        Ok(())
    }
}
