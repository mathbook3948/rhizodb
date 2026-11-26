use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::sync::PoisonError;
use std::sync::RwLock;

use super::blob::BlobRef;
use super::blob::RUN_PAYLOAD_LIMIT;
use super::blob::Role;
use super::blob::RunRef;
use super::blob::decode_checked_run;
use super::blob::decode_run;
use super::blob::item_checksum;
use super::block_file::BlockFile;
use super::cache::Cache;
use super::cache::CachedPage;
use super::cache::CheckedPage;
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
    /// The page at `at`, which a scan reads right before the pages `ahead` names in order. When
    /// the cache lacks it, one read takes it together with the pages of `ahead` that follow it
    /// closely in its extent, and the cache keeps every one of them that checks.
    fn page_ahead(
        &self,
        at: BlockRef,
        ahead: &mut dyn Iterator<Item = BlockRef>,
    ) -> Result<CachedPage, EngineError>;

    fn run_bytes(&self, run: RunRef) -> Result<Vec<u8>, EngineError>;

    /// The block of a run of one block, through the cache, which keeps it once it checks.
    fn run_block(&self, run: RunRef) -> Result<CachedPage, EngineError>;

    fn seed(&self) -> u64;
}

/// Calls `read` with a whole item, in place when it is one run of one block, which the cache
/// then keeps, and checked as [`load_blob`] checks it.
pub(crate) fn read_blob<T>(
    source: &dyn PageSource,
    blob: &BlobRef,
    role: Role,
    read: impl FnOnce(&[u8]) -> T,
) -> Result<T, EngineError> {
    if blob.first.blocks == 1 {
        let held = source.run_block(blob.first)?;
        let (_, decoded) = decode_checked_run(held.header, &held.bytes, blob.first, role)?;
        if decoded.next.at.is_null() {
            whole(blob, decoded.total, decoded.payload)?;
            return Ok(read(decoded.payload));
        }
    }
    Ok(read(&load_blob(source, blob, role)?))
}

fn whole(blob: &BlobRef, total: u64, bytes: &[u8]) -> Result<(), EngineError> {
    if total != blob.length {
        return Err(EngineError::corrupt(format!(
            "the run at {:?} does not belong to its item",
            blob.first.at
        )));
    }
    if bytes.len() as u64 != blob.length || item_checksum(bytes) != blob.checksum {
        return Err(EngineError::corrupt(format!(
            "the item at {:?} fails its checksum",
            blob.first.at
        )));
    }
    Ok(())
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
        let held;
        let read;
        let (_, decoded) = if run.blocks == 1 {
            held = source.run_block(run)?;
            decode_checked_run(held.header, &held.bytes, run, role)?
        } else {
            read = source.run_bytes(run)?;
            decode_run(source.seed(), &read, run, role)?
        };
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

const READ_AHEAD_BLOCKS: u32 = 32;

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
        for offset in 0..blocks {
            self.cache.remove(at.offset(offset)?);
        }
        Ok(())
    }

    /// Writes a one-block page and keeps it in the cache.
    pub(crate) fn write_page(&self, at: BlockRef, bytes: Vec<u8>) -> Result<(), EngineError> {
        self.write(at, &bytes)?;
        let header = PageHeader::open(self.seed, &bytes, at)?;
        self.cache.insert(
            at,
            Arc::new(CheckedPage {
                header,
                bytes: bytes.into_boxed_slice(),
            }),
        );
        Ok(())
    }

    /// Cuts the file after block `blocks` of the extent in slot `slot`, unless it already ends
    /// there or before.
    pub(crate) fn truncate_after(&self, slot: u64, blocks: u32) -> Result<(), EngineError> {
        let end = physical_block(slot, blocks)?;
        if self.file.block_count()? > end {
            self.file.set_block_count(end)?;
        }
        Ok(())
    }

    /// The bytes of the database file.
    pub(crate) fn file_bytes(&self) -> Result<u64, EngineError> {
        Ok(self.file.block_count()? * BLOCK as u64)
    }

    pub(crate) fn cache_hits_and_misses(&self) -> (u64, u64) {
        self.cache.hits_and_misses()
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

impl<B: BlockFile> Pages<B> {
    fn keep(&self, at: BlockRef, bytes: &[u8]) -> Result<CachedPage, EngineError> {
        let header = PageHeader::open(self.seed, bytes, at)?;
        let page = Arc::new(CheckedPage {
            header,
            bytes: bytes.into(),
        });
        self.cache.insert(at, Arc::clone(&page));
        Ok(page)
    }
}

impl<B: BlockFile> PageSource for Pages<B> {
    fn page_ahead(
        &self,
        at: BlockRef,
        ahead: &mut dyn Iterator<Item = BlockRef>,
    ) -> Result<CachedPage, EngineError> {
        if let Some(page) = self.cache.get(at) {
            return Ok(page);
        }
        let mut offsets = Vec::new();
        for next in ahead {
            let offset = next.block().wrapping_sub(at.block());
            if next.extent() != at.extent()
                || next.block() <= at.block()
                || offset >= READ_AHEAD_BLOCKS
                || offsets.last().is_some_and(|last| offset <= *last)
            {
                break;
            }
            offsets.push(offset);
        }
        let blocks = offsets.last().map_or(1, |last| last + 1);
        let mut bytes = vec![0; blocks as usize * BLOCK];
        self.file.read(self.physical(at, blocks)?, &mut bytes)?;
        let page = self.keep(at, &bytes[..BLOCK])?;
        for offset in offsets {
            let start = offset as usize * BLOCK;
            if let (Ok(next), Some(block)) = (at.offset(offset), bytes.get(start..start + BLOCK)) {
                let _ = self.keep(next, block);
            }
        }
        Ok(page)
    }

    fn run_bytes(&self, run: RunRef) -> Result<Vec<u8>, EngineError> {
        let first = self.physical(run.at, run.blocks)?;
        let mut bytes = vec![0; run.blocks as usize * BLOCK];
        self.file.read(first, &mut bytes)?;
        Ok(bytes)
    }

    fn run_block(&self, run: RunRef) -> Result<CachedPage, EngineError> {
        if let Some(page) = self.cache.get(run.at) {
            return Ok(page);
        }
        let mut bytes = vec![0; BLOCK];
        self.file.read(self.physical(run.at, 1)?, &mut bytes)?;
        self.keep(run.at, &bytes)
    }

    fn seed(&self) -> u64 {
        self.seed
    }
}
