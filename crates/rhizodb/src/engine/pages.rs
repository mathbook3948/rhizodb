use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::sync::PoisonError;
use std::sync::RwLock;

use super::blob::BlobRef;
use super::blob::RUN_PAYLOAD_LIMIT;
use super::blob::Role;
use super::blob::RunRef;
use super::blob::decode_run;
use super::blob::item_checksum;
use super::block_file::BlockFile;
use super::cache::Cache;
use super::cache::CachedPage;
use super::error::EngineError;
use super::header::Header;
use super::layout::BLOCK;
use super::layout::BlockRef;
use super::layout::EXTENT_BLOCKS;
use super::layout::ExtentId;
use super::layout::physical_block;
use super::page::PageHeader;

/// Reads checked pages and runs of the database file.
pub(crate) trait PageSource: Send + Sync + fmt::Debug {
    /// The one-block page at `at`, checked against its checksum and its block.
    fn page(&self, at: BlockRef) -> Result<CachedPage, EngineError>;

    /// The blocks of the run `run`, unchecked.
    fn run_bytes(&self, run: RunRef) -> Result<Vec<u8>, EngineError>;

    fn seed(&self) -> u64;
}

/// Reads a whole item from its chain of runs, and fails as corruption unless every run checks
/// and the bytes match the pointer's length and checksum.
pub(crate) fn load_blob(
    source: &dyn PageSource,
    blob: &BlobRef,
    role: Role,
) -> Result<Vec<u8>, EngineError> {
    let length = usize::try_from(blob.length).map_err(|_| EngineError::TooLarge)?;
    let mut bytes = Vec::with_capacity(length.min(RUN_PAYLOAD_LIMIT));
    let mut run = blob.first;
    let most_runs = length / RUN_PAYLOAD_LIMIT + 1;
    for _ in 0..most_runs {
        let blocks = source.run_bytes(run)?;
        let (_, decoded) = decode_run(source.seed(), &blocks, run, role)?;
        if decoded.total != blob.length || bytes.len() + decoded.payload.len() > length {
            return Err(EngineError::corrupt(format!(
                "the run at {:?} does not belong to its item",
                run.at
            )));
        }
        bytes.extend_from_slice(decoded.payload);
        if decoded.next.at.is_null() {
            if bytes.len() != length || item_checksum(&bytes) != blob.checksum {
                return Err(EngineError::corrupt(format!(
                    "the item at {:?} fails its checksum",
                    blob.first.at
                )));
            }
            return Ok(bytes);
        }
        run = decoded.next;
    }
    Err(EngineError::corrupt(format!(
        "the item at {:?} has more runs than its length allows",
        blob.first.at
    )))
}

/// The database file's blocks, found through the extent slots, with a cache of checked pages.
#[derive(Debug)]
pub(crate) struct Pages<B: BlockFile> {
    file: B,
    seed: u64,
    slots: RwLock<HashMap<ExtentId, u64>>,
    cache: Cache,
}

impl<B: BlockFile> Pages<B> {
    pub(crate) fn new(file: B, seed: u64, cache_pages: usize) -> Self {
        Self {
            file,
            seed,
            slots: RwLock::default(),
            cache: Cache::new(cache_pages),
        }
    }

    /// Records that extent `id` lies in slot `slot`, which never changes once recorded.
    pub(crate) fn place_extent(&self, id: ExtentId, slot: u64) {
        self.slots
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id, slot);
    }

    fn physical(&self, at: BlockRef, blocks: u32) -> Result<u64, EngineError> {
        if blocks == 0
            || at
                .block()
                .checked_add(blocks)
                .is_none_or(|end| end > EXTENT_BLOCKS)
        {
            return Err(EngineError::corrupt(format!(
                "{blocks} blocks at {at:?} cross the end of their extent"
            )));
        }
        let slot = *self
            .slots
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&at.extent())
            .ok_or_else(|| {
                EngineError::corrupt(format!("{at:?} lies in an extent the table does not hold"))
            })?;
        physical_block(slot, at.block())
    }

    /// Writes the blocks of a page or run that starts at `at`.
    pub(crate) fn write(&self, at: BlockRef, bytes: &[u8]) -> Result<(), EngineError> {
        let blocks = u32::try_from(bytes.len() / BLOCK).map_err(|_| EngineError::TooLarge)?;
        let first = self.physical(at, blocks)?;
        self.file.write(first, bytes)?;
        Ok(())
    }

    /// Writes a one-block page and keeps it in the cache.
    pub(crate) fn write_page(&self, at: BlockRef, bytes: Vec<u8>) -> Result<(), EngineError> {
        self.write(at, &bytes)?;
        let header = PageHeader::open(self.seed, &bytes, at)?;
        self.cache.insert(at, (header, Arc::from(bytes)));
        Ok(())
    }

    pub(crate) fn sync(&self) -> Result<(), EngineError> {
        self.file.sync()?;
        Ok(())
    }

    pub(crate) fn write_header(&self, header: &Header) -> Result<(), EngineError> {
        self.file.write(header.slot(), &header.encode())?;
        self.file.sync()?;
        Ok(())
    }
}

impl<B: BlockFile> PageSource for Pages<B> {
    fn page(&self, at: BlockRef) -> Result<CachedPage, EngineError> {
        if let Some(page) = self.cache.get(at) {
            return Ok(page);
        }
        let mut bytes = vec![0; BLOCK];
        self.file.read(self.physical(at, 1)?, &mut bytes)?;
        let header = PageHeader::open(self.seed, &bytes, at)?;
        let page = (header, Arc::from(bytes));
        self.cache.insert(at, page.clone());
        Ok(page)
    }

    fn run_bytes(&self, run: RunRef) -> Result<Vec<u8>, EngineError> {
        let first = self.physical(run.at, run.blocks)?;
        let mut bytes = vec![0; run.blocks as usize * BLOCK];
        self.file.read(first, &mut bytes)?;
        Ok(bytes)
    }

    fn seed(&self) -> u64 {
        self.seed
    }
}
