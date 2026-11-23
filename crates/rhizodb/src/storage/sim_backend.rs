use std::io;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use redb::BackendError;
use redb::StorageBackend;

use crate::engine::block_file::BlockFile;
use crate::engine::block_file::BlockSize;
use crate::engine::block_file::FileSystem;
use crate::engine::block_file::SECTOR_SIZE;
use crate::engine::sim_fs::SimFile;
use crate::engine::sim_fs::SimFs;

use super::StorageError;
use super::redb_backend::RedbStorage;

const FILE: &str = "graph.redb";

/// A redb file on a [`SimFs`], in blocks of one sector. A write or read that does not cover
/// whole sectors reads the sectors it touches first, so a torn write tears what redb wrote.
#[derive(Debug)]
pub(crate) struct SimBackend {
    file: SimFile,
    capacity: Option<Arc<AtomicU64>>,
}

fn sector() -> BlockSize {
    BlockSize::new(SECTOR_SIZE).unwrap()
}

fn sectors(offset: u64, length: usize) -> io::Result<(u64, usize)> {
    let sector = SECTOR_SIZE as u64;
    let end = offset
        .checked_add(length as u64)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "range past u64"))?;
    let first = offset / sector;
    let last = end.div_ceil(sector);
    Ok((first, ((last - first) * sector) as usize))
}

impl SimBackend {
    /// The database file on `fs`, created when there is none.
    pub(crate) fn open(fs: &SimFs) -> io::Result<Self> {
        let file = match fs.open(FILE, sector()) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let file = fs.create(FILE, sector())?;
                fs.sync_names()?;
                file
            }
            Err(error) => return Err(error),
        };
        Ok(Self {
            file,
            capacity: None,
        })
    }

    /// Fails a write or a growth past the byte length that `capacity` holds with
    /// [`io::ErrorKind::StorageFull`], as a full disk does, while reads keep working.
    pub(crate) fn with_capacity(mut self, capacity: Arc<AtomicU64>) -> Self {
        self.capacity = Some(capacity);
        self
    }

    fn fits(&self, end: u64) -> io::Result<()> {
        match &self.capacity {
            Some(capacity) if end > capacity.load(Ordering::Relaxed) => Err(io::Error::new(
                io::ErrorKind::StorageFull,
                "no space left on the simulated device",
            )),
            _ => Ok(()),
        }
    }
}

impl StorageBackend for SimBackend {
    fn len(&self) -> Result<u64, BackendError> {
        Ok(self.file.block_count()? * SECTOR_SIZE as u64)
    }

    fn read(&self, offset: u64, out: &mut [u8]) -> Result<(), BackendError> {
        let (first, covered) = sectors(offset, out.len())?;
        let mut buffer = vec![0; covered];
        self.file.read(first, &mut buffer)?;
        let skip = (offset - first * SECTOR_SIZE as u64) as usize;
        out.copy_from_slice(&buffer[skip..skip + out.len()]);
        Ok(())
    }

    fn set_len(&self, len: u64) -> Result<(), BackendError> {
        if !len.is_multiple_of(SECTOR_SIZE as u64) {
            return Err(
                io::Error::new(io::ErrorKind::InvalidInput, "length is not whole sectors").into(),
            );
        }
        if self.capacity.is_some() && len > self.len()? {
            self.fits(len)?;
        }
        Ok(self.file.set_block_count(len / SECTOR_SIZE as u64)?)
    }

    fn sync_data(&self) -> Result<(), BackendError> {
        Ok(self.file.sync()?)
    }

    fn write(&self, offset: u64, data: &[u8]) -> Result<(), BackendError> {
        let (first, covered) = sectors(offset, data.len())?;
        self.fits(first * SECTOR_SIZE as u64 + covered as u64)?;
        let skip = (offset - first * SECTOR_SIZE as u64) as usize;
        if skip == 0 && covered == data.len() {
            return Ok(self.file.write(first, data)?);
        }
        let mut buffer = vec![0; covered];
        let present = self.file.block_count()?;
        let readable = present
            .saturating_sub(first)
            .min((covered / SECTOR_SIZE) as u64);
        self.file
            .read(first, &mut buffer[..readable as usize * SECTOR_SIZE])?;
        buffer[skip..skip + data.len()].copy_from_slice(data);
        Ok(self.file.write(first, &buffer)?)
    }
}

/// A redb database on `fs`.
pub(crate) fn storage(fs: &SimFs) -> Result<RedbStorage, StorageError> {
    RedbStorage::with_backend(SimBackend::open(fs).map_err(StorageError::new)?)
}

/// The byte length of the database file on `fs`.
pub(crate) fn length(fs: &SimFs) -> io::Result<u64> {
    Ok(fs.open(FILE, sector())?.block_count()? * SECTOR_SIZE as u64)
}

/// A redb database on `fs` whose file cannot grow past the byte length `capacity` holds.
pub(crate) fn storage_with_capacity(
    fs: &SimFs,
    capacity: Arc<AtomicU64>,
) -> Result<RedbStorage, StorageError> {
    RedbStorage::with_backend(
        SimBackend::open(fs)
            .map_err(StorageError::new)?
            .with_capacity(capacity),
    )
}
