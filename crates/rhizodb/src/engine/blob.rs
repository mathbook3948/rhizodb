use super::codec::Reader;
use super::codec::checksum;
use super::codec::put_u32;
use super::codec::put_u64;
use super::error::EngineError;
use super::layout::BLOCK;
use super::layout::BlockRef;
use super::layout::EXTENT_BLOCKS;
use super::layout::TableId;
use super::page::PAGE_HEADER;
use super::page::PageHeader;
use super::page::PageKind;

/// The bytes of a run before its payload.
pub(crate) const RUN_HEADER: usize = PAGE_HEADER + 109;

/// The most payload one run holds: a whole extent less its header.
pub(crate) const RUN_PAYLOAD_LIMIT: usize = EXTENT_BLOCKS as usize * BLOCK - RUN_HEADER;

const OWNER_HEAD: usize = 64;

/// What a run's bytes are part of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Role {
    Value,
    Key,
    Catalog,
}

impl Role {
    fn code(self) -> u8 {
        match self {
            Role::Value => 0,
            Role::Key => 1,
            Role::Catalog => 2,
        }
    }

    fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            0 => Role::Value,
            1 => Role::Key,
            2 => Role::Catalog,
            _ => return None,
        })
    }

    fn kind(self) -> PageKind {
        match self {
            Role::Value | Role::Key => PageKind::ValueRun,
            Role::Catalog => PageKind::Catalog,
        }
    }
}

/// Where a run of blocks lies and the LSN its pages carry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub(crate) struct RunRef {
    pub(crate) at: BlockRef,
    pub(crate) blocks: u32,
    pub(crate) lsn: u64,
}

/// A stored item too long for its page: a value, a long key, or the catalog. It is a chain of
/// runs, each inside one extent, and its checksum covers all of its bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub(crate) struct BlobRef {
    pub(crate) first: RunRef,
    pub(crate) length: u64,
    pub(crate) checksum: u64,
}

/// The run header, which names the item and the key it belongs to, so a stray run can be told
/// apart from the one a pointer expects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Run<'a> {
    pub(crate) total: u64,
    pub(crate) next: RunRef,
    pub(crate) role: Role,
    pub(crate) owner_length: u32,
    pub(crate) owner_head: &'a [u8],
    pub(crate) owner_hash: u64,
    pub(crate) payload: &'a [u8],
}

/// The blocks a run with `payload` bytes takes.
pub(crate) fn run_blocks(payload: usize) -> u32 {
    (RUN_HEADER + payload).div_ceil(BLOCK) as u32
}

/// The payload of each run of an item of `length` bytes: whole extents of payload, then the
/// rest.
pub(crate) fn split_runs(length: usize) -> Vec<usize> {
    let mut runs = vec![RUN_PAYLOAD_LIMIT; length / RUN_PAYLOAD_LIMIT];
    if !length.is_multiple_of(RUN_PAYLOAD_LIMIT) || runs.is_empty() {
        runs.push(length % RUN_PAYLOAD_LIMIT);
    }
    runs
}

/// The owner fields of a run: the key's length, its first bytes, and its hash.
pub(crate) fn owner_of(key: &[u8]) -> (u32, &[u8], u64) {
    (
        u32::try_from(key.len()).unwrap_or(u32::MAX),
        &key[..key.len().min(OWNER_HEAD)],
        checksum(0, &[key]),
    )
}

/// The checksum of a whole item, which its pointer records.
pub(crate) fn item_checksum(bytes: &[u8]) -> u64 {
    checksum(0x0B10_B000, &[bytes])
}

pub(crate) struct RunSpec<'a> {
    pub(crate) role: Role,
    pub(crate) table: TableId,
    pub(crate) at: BlockRef,
    pub(crate) lsn: u64,
    pub(crate) generation: u64,
    pub(crate) total: u64,
    pub(crate) next: RunRef,
    pub(crate) owner: &'a [u8],
    pub(crate) payload: &'a [u8],
}

impl RunSpec<'_> {
    /// The run's blocks, sealed.
    pub(crate) fn encode(&self, seed: u64) -> Result<Vec<u8>, EngineError> {
        let blocks = run_blocks(self.payload.len());
        if blocks > EXTENT_BLOCKS {
            return Err(EngineError::TooLarge);
        }
        let mut out = vec![0; PAGE_HEADER];
        put_u64(&mut out, self.total);
        put_u32(
            &mut out,
            u32::try_from(self.payload.len()).map_err(|_| EngineError::TooLarge)?,
        );
        put_u64(&mut out, self.next.at.raw());
        put_u32(&mut out, self.next.blocks);
        put_u64(&mut out, self.next.lsn);
        out.push(self.role.code());
        let (length, head, hash) = owner_of(self.owner);
        put_u32(&mut out, length);
        out.extend_from_slice(head);
        out.resize(PAGE_HEADER + 37 + OWNER_HEAD, 0);
        put_u64(&mut out, hash);
        out.resize(RUN_HEADER, 0);
        out.extend_from_slice(self.payload);
        out.resize(blocks as usize * BLOCK, 0);
        PageHeader {
            kind: self.role.kind(),
            format: 1,
            table: self.table,
            level: 0,
            flags: 0,
            cells: 0,
            free: 0,
            at: self.at,
            lsn: self.lsn,
            generation: self.generation,
        }
        .seal(seed, &mut out)?;
        Ok(out)
    }
}

/// Checks the run read from `run` and returns its header and payload.
pub(crate) fn decode_run<'a>(
    seed: u64,
    bytes: &'a [u8],
    run: RunRef,
    role: Role,
) -> Result<(PageHeader, Run<'a>), EngineError> {
    if bytes.len() != run.blocks as usize * BLOCK {
        return Err(EngineError::corrupt(format!(
            "the run at {:?} was read with the wrong length",
            run.at
        )));
    }
    let header = PageHeader::open(seed, bytes, run.at)?;
    header.expect(role.kind(), run.lsn)?;
    let mut reader = Reader::new(&bytes[PAGE_HEADER..], "a run header");
    let total = reader.u64()?;
    let payload_length = reader.u32()? as usize;
    let next = RunRef {
        at: BlockRef::from_raw(reader.u64()?)?,
        blocks: reader.u32()?,
        lsn: reader.u64()?,
    };
    let found_role = Role::from_code(reader.u8()?).ok_or_else(|| reader.bad("role"))?;
    let owner_length = reader.u32()?;
    let owner_head = reader.take(OWNER_HEAD)?;
    let owner_hash = reader.u64()?;
    let head_length = (owner_length as usize).min(OWNER_HEAD);
    if found_role != role
        || run_blocks(payload_length) != run.blocks
        || owner_head[head_length..].iter().any(|byte| *byte != 0)
        || (next.at.is_null() && (next.blocks, next.lsn) != (0, 0))
        || (!next.at.is_null() && (next.blocks == 0 || next.blocks > EXTENT_BLOCKS))
    {
        return Err(EngineError::corrupt(format!(
            "the run at {:?} has an invalid header",
            run.at
        )));
    }
    let payload = bytes
        .get(RUN_HEADER..RUN_HEADER + payload_length)
        .ok_or_else(|| EngineError::corrupt("a run's payload is cut short"))?;
    Ok((
        header,
        Run {
            total,
            next,
            role,
            owner_length,
            owner_head: &owner_head[..head_length],
            owner_hash,
            payload,
        },
    ))
}

#[cfg(test)]
#[path = "blob.test.rs"]
mod tests;
