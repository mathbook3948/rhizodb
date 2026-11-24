use std::fmt;

use crate::storage::Table;

use super::error::EngineError;

/// The size of every block of the database file.
pub(crate) const BLOCK: usize = 4096;

/// The size of every block of a log segment, one sector, so an append never rewrites a sector
/// that holds an earlier synced record.
pub(crate) const WAL_BLOCK: usize = 512;

/// The blocks of one extent, 4 MiB.
pub(crate) const EXTENT_BLOCKS: u32 = 1024;

/// The header slots at the start of the database file, blocks 0 and 1.
pub(crate) const HEADER_SLOTS: u64 = 2;

const BLOCK_BITS: u32 = 20;

const EXTENT_ID_LIMIT: u64 = 1 << 44;

/// An extent's identity, which is never reused. Zero names no extent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct ExtentId(pub(crate) u64);

impl ExtentId {
    pub(crate) const NONE: ExtentId = ExtentId(0);

    pub(crate) fn is_none(self) -> bool {
        self.0 == 0
    }
}

/// A block named by its extent and its place in the extent: the extent id in the high 44 bits and
/// the block in the low 20. Zero refers to nothing.
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct BlockRef(u64);

impl BlockRef {
    pub(crate) const NULL: BlockRef = BlockRef(0);

    pub(crate) fn new(extent: ExtentId, block: u32) -> Result<Self, EngineError> {
        if extent.is_none() || extent.0 >= EXTENT_ID_LIMIT || block >= EXTENT_BLOCKS {
            return Err(EngineError::corrupt(format!(
                "no block {block} of extent {}",
                extent.0
            )));
        }
        Ok(Self((extent.0 << BLOCK_BITS) | u64::from(block)))
    }

    /// Reads a stored reference, which must be null or name a block inside an extent.
    pub(crate) fn from_raw(raw: u64) -> Result<Self, EngineError> {
        let reference = Self(raw);
        if raw != 0 && (reference.extent().is_none() || reference.block() >= EXTENT_BLOCKS) {
            return Err(EngineError::corrupt(format!(
                "block reference {raw:#x} names no block"
            )));
        }
        Ok(reference)
    }

    pub(crate) fn raw(self) -> u64 {
        self.0
    }

    pub(crate) fn is_null(self) -> bool {
        self.0 == 0
    }

    pub(crate) fn extent(self) -> ExtentId {
        ExtentId(self.0 >> BLOCK_BITS)
    }

    pub(crate) fn block(self) -> u32 {
        (self.0 & ((1 << BLOCK_BITS) - 1)) as u32
    }

    /// The reference `blocks` further into the same extent.
    pub(crate) fn offset(self, blocks: u32) -> Result<Self, EngineError> {
        let block = self
            .block()
            .checked_add(blocks)
            .ok_or(EngineError::TooLarge)?;
        Self::new(self.extent(), block)
    }
}

impl fmt::Debug for BlockRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "BlockRef({}:{})", self.extent().0, self.block())
    }
}

/// The file block where block `block` of the extent in slot `slot` lies.
pub(crate) fn physical_block(slot: u64, block: u32) -> Result<u64, EngineError> {
    slot.checked_mul(u64::from(EXTENT_BLOCKS))
        .and_then(|start| start.checked_add(HEADER_SLOTS + u64::from(block)))
        .ok_or(EngineError::TooLarge)
}

/// A table's stable number in the file and the log. 1 to 255 are the engine's own, 256 to 511
/// the graph's tables, and the rest reserved.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct TableId(pub(crate) u16);

impl TableId {
    pub(crate) fn of(table: Table) -> TableId {
        TableId(match table {
            Table::Metadata => 0x100,
            Table::TokenByName => 0x101,
            Table::TokenById => 0x102,
            Table::Nodes => 0x103,
            Table::Relationships => 0x104,
            Table::Outgoing => 0x105,
            Table::Incoming => 0x106,
            Table::LabelIndex => 0x107,
            Table::Indexes => 0x108,
            Table::KeywordPostings => 0x109,
            Table::KeywordDocuments => 0x10A,
            Table::KeywordTotals => 0x10B,
            Table::Vectors => 0x10C,
            Table::PropertyEntries => 0x10D,
            Table::NodeChunks => 0x10E,
            Table::LabelSets => 0x10F,
            Table::LabelSetByTokens => 0x110,
            Table::NodeColumns => 0x111,
            Table::Adjacency => 0x112,
            Table::RelationshipChunks => 0x113,
        })
    }

    /// The graph table a stored id names, which fails for an id no table of this build has.
    pub(crate) fn table(self) -> Result<Table, EngineError> {
        Table::ALL
            .into_iter()
            .find(|table| TableId::of(*table) == self)
            .ok_or_else(|| EngineError::Unsupported(format!("table id {:#x}", self.0)))
    }
}

#[cfg(test)]
#[path = "layout.test.rs"]
mod tests;
