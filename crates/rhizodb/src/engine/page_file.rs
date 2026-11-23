use std::fmt;
use std::io;

/// The size of every page the engine reads and writes.
pub(crate) const PAGE_SIZE: usize = 4096;

pub(crate) type Page = [u8; PAGE_SIZE];

/// A file read and written in whole pages addressed by number.
pub(crate) trait PageFile: Send + Sync + fmt::Debug {
    /// Fills `pages` with the pages from `first` on, and fails when one of them lies past the end.
    fn read(&self, first: u64, pages: &mut [Page]) -> io::Result<()>;

    /// Writes `pages` from page `first` on. A write past the end extends the file, and the pages
    /// it skips read as zeros.
    fn write(&self, first: u64, pages: &[Page]) -> io::Result<()>;

    /// The number of whole pages in the file.
    fn page_count(&self) -> io::Result<u64>;

    /// Truncates the file to `count` pages, or extends it with zeroed pages.
    fn set_page_count(&self, count: u64) -> io::Result<()>;

    /// Returns once the file's pages and page count survive a power loss.
    fn sync(&self) -> io::Result<()>;
}

/// A flat set of named page files, such as the database file and its WAL segments.
pub(crate) trait FileSystem: Send + Sync {
    type File: PageFile;

    /// Creates an empty file, and fails when `name` exists.
    fn create(&self, name: &str) -> io::Result<Self::File>;

    /// Opens an existing file, and fails with [`io::ErrorKind::NotFound`] when there is none.
    fn open(&self, name: &str) -> io::Result<Self::File>;

    /// Renames `from` to `to`, replacing a file named `to`.
    fn rename(&self, from: &str, to: &str) -> io::Result<()>;

    fn remove(&self, name: &str) -> io::Result<()>;

    /// The names of every file, in ascending order.
    fn names(&self) -> io::Result<Vec<String>>;

    /// Returns once every create, rename, and remove so far survives a power loss.
    fn sync_names(&self) -> io::Result<()>;
}

pub(crate) fn byte_offset(page: u64) -> io::Result<u64> {
    page.checked_mul(PAGE_SIZE as u64)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "page number out of range"))
}

#[cfg(test)]
#[path = "page_file.test.rs"]
mod tests;
