use std::fs;
use std::io;
use std::path::Path;

/// How the read workloads meet the pages of the database they read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Memory {
    /// The bytes of database pages each engine may keep in its own cache, for the engines that
    /// have a setting for it. `None` keeps each engine's default.
    pub cache_bytes: Option<u64>,
    /// Whether every run of a read workload starts on a database opened for it, after the
    /// operating system dropped the cached pages of its files, so the first read of each page
    /// goes to the disk.
    pub cold: bool,
}

/// Drops the operating system's cached pages of every file under `dir`. The files must be
/// closed by the engine that wrote them, so that no page is dirty or mapped.
#[cfg(target_os = "linux")]
pub fn evict(dir: &Path) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            evict(&entry.path())?;
            continue;
        }
        let file = fs::File::open(entry.path())?;
        file.sync_all()?;
        rustix::fs::fadvise(&file, 0, None, rustix::fs::Advice::DontNeed)?;
    }
    Ok(())
}

/// Cold runs need the page cache of Linux dropped file by file, which other systems lack.
#[cfg(not(target_os = "linux"))]
pub fn evict(_dir: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "cold runs drop cached pages with posix_fadvise, which this benchmark uses only on Linux",
    ))
}

/// The bytes this process has read from storage devices since it started, which reads served
/// by the page cache do not count, or `None` where the system does not say.
pub fn disk_read_bytes() -> Option<u64> {
    let text = fs::read_to_string("/proc/self/io").ok()?;
    text.lines()
        .find_map(|line| line.strip_prefix("read_bytes:"))
        .and_then(|value| value.trim().parse().ok())
}

#[cfg(test)]
#[path = "memory.test.rs"]
mod tests;
