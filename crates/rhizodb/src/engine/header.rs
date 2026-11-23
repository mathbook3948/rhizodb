use super::codec::Reader;
use super::codec::checksum;
use super::codec::put_u16;
use super::codec::put_u32;
use super::codec::put_u64;
use super::codec::put_u128;
use super::error::EngineError;
use super::layout::BLOCK;
use super::layout::BlockRef;
use super::layout::EXTENT_BLOCKS;
use super::layout::ExtentId;

pub(crate) const MAGIC: [u8; 8] = *b"RHIZODB\0";

/// The engine format this build writes. A file of another major or a newer minor is refused.
pub(crate) const FORMAT_MAJOR: u16 = 1;
pub(crate) const FORMAT_MINOR: u16 = 0;

/// The extents a checkpoint appends to, one per kind of block, whose written lengths the header
/// records.
pub(crate) const OPEN_EXTENTS: usize = 4;

const CLEAN: u32 = 1;
const KNOWN_FLAGS: u32 = CLEAN;

const CHECKSUM_AT: usize = BLOCK - 8;

/// An extent a checkpoint appends to. Blocks past `written` hold nothing a header reaches, so a
/// later checkpoint overwrites them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct OpenExtent {
    pub(crate) id: ExtentId,
    pub(crate) slot: u64,
    pub(crate) written: u32,
}

/// The log that holds the commits after the checkpoint: the first segment still needed and the
/// salt every segment and record of this log carries.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct WalIdentity {
    pub(crate) first_segment: u64,
    pub(crate) salt: u64,
}

/// One header slot. The valid slot with the higher generation describes the database.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Header {
    pub(crate) minor: u16,
    pub(crate) database_id: u128,
    pub(crate) shard_id: u64,
    pub(crate) generation: u64,
    pub(crate) checkpoint_lsn: u64,
    pub(crate) checkpoint_hlc: u64,
    pub(crate) catalog: BlockRef,
    pub(crate) catalog_lsn: u64,
    pub(crate) next_extent: ExtentId,
    pub(crate) open: [OpenExtent; OPEN_EXTENTS],
    pub(crate) wal: WalIdentity,
    /// The database was closed cleanly: the checkpoint holds every commit and no log is needed.
    pub(crate) clean: bool,
}

impl Header {
    /// The slot this header is written to, which alternates with the generation.
    pub(crate) fn slot(&self) -> u64 {
        self.generation % 2
    }

    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(BLOCK);
        out.extend_from_slice(&MAGIC);
        put_u16(&mut out, FORMAT_MAJOR);
        put_u16(&mut out, self.minor);
        put_u32(&mut out, BLOCK as u32);
        put_u32(&mut out, EXTENT_BLOCKS);
        put_u32(&mut out, 0);
        put_u128(&mut out, self.database_id);
        put_u64(&mut out, self.shard_id);
        put_u64(&mut out, self.generation);
        put_u64(&mut out, self.checkpoint_lsn);
        put_u64(&mut out, self.checkpoint_hlc);
        put_u64(&mut out, self.catalog.raw());
        put_u64(&mut out, self.catalog_lsn);
        put_u64(&mut out, self.next_extent.0);
        for open in &self.open {
            put_u64(&mut out, open.id.0);
            put_u64(&mut out, open.slot);
            put_u32(&mut out, open.written);
            put_u32(&mut out, 0);
        }
        put_u64(&mut out, self.wal.first_segment);
        put_u64(&mut out, self.wal.salt);
        put_u32(&mut out, if self.clean { CLEAN } else { 0 });
        out.resize(CHECKSUM_AT, 0);
        let sum = checksum(0, &[&out]);
        put_u64(&mut out, sum);
        out
    }

    /// Decodes one slot. A wrong magic, size, or checksum fails as corruption, and a format this
    /// build cannot read fails as unsupported.
    pub(crate) fn decode(block: &[u8]) -> Result<Self, EngineError> {
        if block.len() != BLOCK {
            return Err(EngineError::corrupt("a header slot is not one block"));
        }
        let (body, stored) = block.split_at(CHECKSUM_AT);
        let mut reader = Reader::new(body, "a header slot");
        if reader.take(MAGIC.len())? != MAGIC {
            return Err(EngineError::corrupt(
                "a header slot does not start with the RhizoDB magic",
            ));
        }
        let mut sum = Reader::new(stored, "a header checksum");
        if sum.u64()? != checksum(0, &[body]) {
            return Err(EngineError::corrupt("a header slot fails its checksum"));
        }
        let major = reader.u16()?;
        let minor = reader.u16()?;
        if major != FORMAT_MAJOR || minor > FORMAT_MINOR {
            return Err(EngineError::Unsupported(format!(
                "engine format {major}.{minor}, and this build reads {FORMAT_MAJOR}.0 to \
                 {FORMAT_MAJOR}.{FORMAT_MINOR}"
            )));
        }
        let block_size = reader.u32()?;
        let extent_blocks = reader.u32()?;
        if block_size as usize != BLOCK || extent_blocks != EXTENT_BLOCKS {
            return Err(EngineError::Unsupported(format!(
                "blocks of {block_size} bytes in extents of {extent_blocks} blocks"
            )));
        }
        reader.u32()?;
        let database_id = reader.u128()?;
        let shard_id = reader.u64()?;
        let generation = reader.u64()?;
        let checkpoint_lsn = reader.u64()?;
        let checkpoint_hlc = reader.u64()?;
        let catalog = BlockRef::from_raw(reader.u64()?)?;
        let catalog_lsn = reader.u64()?;
        let next_extent = ExtentId(reader.u64()?);
        let mut open = [OpenExtent::default(); OPEN_EXTENTS];
        for entry in &mut open {
            *entry = OpenExtent {
                id: ExtentId(reader.u64()?),
                slot: reader.u64()?,
                written: reader.u32()?,
            };
            reader.u32()?;
            if entry.written > EXTENT_BLOCKS
                || (!entry.id.is_none() && entry.id >= next_extent)
                || (entry.id.is_none() && (entry.slot, entry.written) != (0, 0))
            {
                return Err(reader.bad("open extent"));
            }
        }
        let wal = WalIdentity {
            first_segment: reader.u64()?,
            salt: reader.u64()?,
        };
        let flags = reader.u32()?;
        if flags & !KNOWN_FLAGS != 0 {
            return Err(EngineError::Unsupported(format!("header flags {flags:#x}")));
        }
        if reader.rest().iter().any(|byte| *byte != 0) {
            return Err(reader.bad("reserved area"));
        }
        if generation == 0 || next_extent.is_none() || catalog_lsn > checkpoint_lsn {
            return Err(reader.bad("generation, extent counter, or catalog LSN"));
        }
        Ok(Self {
            minor,
            database_id,
            shard_id,
            generation,
            checkpoint_lsn,
            checkpoint_hlc,
            catalog,
            catalog_lsn,
            next_extent,
            open,
            wal,
            clean: flags & CLEAN != 0,
        })
    }
}

/// Decodes the slot in block `slot`, which must hold a generation of its parity.
pub(crate) fn decode_slot(block: &[u8], slot: u64) -> Result<Header, EngineError> {
    let header = Header::decode(block)?;
    if header.slot() != slot {
        return Err(EngineError::corrupt(format!(
            "header slot {slot} holds generation {}",
            header.generation
        )));
    }
    Ok(header)
}

/// The header a database opens with: the valid slot of the higher generation. Two valid slots
/// must belong to one database and differ in generation, and a slot of a format this build cannot
/// read refuses the file.
pub(crate) fn choose(
    first: Result<Header, EngineError>,
    second: Result<Header, EngineError>,
) -> Result<Header, EngineError> {
    match (first, second) {
        (Ok(first), Ok(second)) => {
            if first.database_id != second.database_id || first.shard_id != second.shard_id {
                return Err(EngineError::corrupt(
                    "the header slots belong to different databases",
                ));
            }
            if first.generation == second.generation {
                return Err(EngineError::corrupt(
                    "both header slots hold the same generation",
                ));
            }
            Ok(if first.generation > second.generation {
                first
            } else {
                second
            })
        }
        (Ok(header), Err(other)) | (Err(other), Ok(header)) => {
            if matches!(other, EngineError::Unsupported(_)) {
                return Err(other);
            }
            Ok(header)
        }
        (Err(first), Err(second)) => Err(match (first, second) {
            (unsupported @ EngineError::Unsupported(_), _)
            | (_, unsupported @ EngineError::Unsupported(_)) => unsupported,
            (first, second) => {
                EngineError::corrupt(format!("no header slot is valid: {first}; {second}"))
            }
        }),
    }
}

#[cfg(test)]
#[path = "header.test.rs"]
mod tests;
