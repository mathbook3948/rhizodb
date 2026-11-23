use super::codec::Reader;
use super::codec::checksum;
use super::error::EngineError;
use super::layout::BLOCK;
use super::layout::BlockRef;
use super::layout::TableId;

/// The bytes every page starts with.
pub(crate) const PAGE_HEADER: usize = 48;

const CHECKSUM_AT: usize = 40;

/// What a page holds. A page of one kind is never read as another.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PageKind {
    Leaf,
    Branch,
    /// The blocks of a value too long to keep in its leaf, or a part of one.
    ValueRun,
    /// The table roots and the extent table a header points to.
    Catalog,
}

impl PageKind {
    fn code(self) -> u8 {
        match self {
            PageKind::Leaf => 1,
            PageKind::Branch => 2,
            PageKind::ValueRun => 3,
            PageKind::Catalog => 4,
        }
    }

    fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            1 => PageKind::Leaf,
            2 => PageKind::Branch,
            3 => PageKind::ValueRun,
            4 => PageKind::Catalog,
            _ => return None,
        })
    }
}

/// The header of a page. A page is one block, or for a value run or a catalog several blocks in a
/// row of one extent, and its checksum covers all of them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PageHeader {
    pub(crate) kind: PageKind,
    pub(crate) format: u8,
    pub(crate) table: TableId,
    pub(crate) level: u8,
    pub(crate) flags: u8,
    pub(crate) cells: u16,
    pub(crate) free: u16,
    /// The block the page was written to, which a read checks, so a misplaced write is caught.
    pub(crate) at: BlockRef,
    /// The LSN of the checkpoint that wrote the page, which the pointer to it repeats.
    pub(crate) lsn: u64,
    pub(crate) generation: u64,
}

/// The checksum seed of a database's pages, so a page copied from another database fails.
pub(crate) fn page_seed(database_id: u128) -> u64 {
    (database_id as u64) ^ ((database_id >> 64) as u64)
}

fn page_checksum(seed: u64, page: &[u8]) -> u64 {
    checksum(
        seed,
        &[&page[..CHECKSUM_AT], &page[PAGE_HEADER.min(page.len())..]],
    )
}

impl PageHeader {
    /// Writes this header and the checksum into `page`, whose body is already in place.
    pub(crate) fn seal(&self, seed: u64, page: &mut [u8]) -> Result<(), EngineError> {
        if page.len() < BLOCK || !page.len().is_multiple_of(BLOCK) {
            return Err(EngineError::corrupt("a page is not whole blocks"));
        }
        page[0] = self.kind.code();
        page[1] = self.format;
        page[2..4].copy_from_slice(&self.table.0.to_le_bytes());
        page[4] = self.level;
        page[5] = self.flags;
        page[6..8].copy_from_slice(&self.cells.to_le_bytes());
        page[8..10].copy_from_slice(&self.free.to_le_bytes());
        page[10..12].fill(0);
        page[12..20].copy_from_slice(&self.at.raw().to_le_bytes());
        page[20..28].copy_from_slice(&self.lsn.to_le_bytes());
        page[28..36].copy_from_slice(&self.generation.to_le_bytes());
        page[36..40].fill(0);
        let sum = page_checksum(seed, page);
        page[CHECKSUM_AT..PAGE_HEADER].copy_from_slice(&sum.to_le_bytes());
        Ok(())
    }

    /// Checks the page read from `at` and returns its header. A page that fails its checksum, or
    /// that was written for another block, is corrupt.
    pub(crate) fn open(seed: u64, page: &[u8], at: BlockRef) -> Result<Self, EngineError> {
        if page.len() < BLOCK || !page.len().is_multiple_of(BLOCK) {
            return Err(EngineError::corrupt("a page is not whole blocks"));
        }
        let mut reader = Reader::new(&page[..PAGE_HEADER], "a page header");
        let kind = reader.u8()?;
        let format = reader.u8()?;
        let table = TableId(reader.u16()?);
        let level = reader.u8()?;
        let flags = reader.u8()?;
        let cells = reader.u16()?;
        let free = reader.u16()?;
        let reserved = reader.u16()?;
        let written_at = reader.u64()?;
        let lsn = reader.u64()?;
        let generation = reader.u64()?;
        let reserved_tail = reader.u32()?;
        let stored = reader.u64()?;
        if stored != page_checksum(seed, page) {
            return Err(EngineError::corrupt(format!(
                "the page at {at:?} fails its checksum"
            )));
        }
        if written_at != at.raw() {
            return Err(EngineError::corrupt(format!(
                "the page at {at:?} was written for {:?}",
                BlockRef::from_raw(written_at).unwrap_or(BlockRef::NULL)
            )));
        }
        let kind = PageKind::from_code(kind)
            .ok_or_else(|| EngineError::corrupt(format!("the page at {at:?} has kind {kind}")))?;
        if reserved != 0 || reserved_tail != 0 || usize::from(free) > page.len().min(BLOCK) {
            return Err(EngineError::corrupt(format!(
                "the page at {at:?} has an invalid header"
            )));
        }
        Ok(Self {
            kind,
            format,
            table,
            level,
            flags,
            cells,
            free,
            at,
            lsn,
            generation,
        })
    }

    /// Fails unless the page is of `kind` and the LSN its pointer recorded.
    pub(crate) fn expect(&self, kind: PageKind, lsn: u64) -> Result<(), EngineError> {
        if self.kind != kind {
            return Err(EngineError::corrupt(format!(
                "the page at {:?} is a {:?} where a {kind:?} was expected",
                self.at, self.kind
            )));
        }
        if self.lsn != lsn {
            return Err(EngineError::corrupt(format!(
                "the page at {:?} has LSN {} where its pointer recorded {lsn}",
                self.at, self.lsn
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "page.test.rs"]
mod tests;
