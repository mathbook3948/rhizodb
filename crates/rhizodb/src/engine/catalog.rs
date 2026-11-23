use std::collections::BTreeMap;

use super::codec::Reader;
use super::codec::put_u16;
use super::codec::put_u32;
use super::codec::put_u64;
use super::error::EngineError;
use super::extent::ExtentEntry;
use super::extent::ExtentState;
use super::extent::ExtentTable;
use super::extent::Purpose;
use super::header::OPEN_EXTENTS;
use super::layout::BlockRef;
use super::layout::ExtentId;
use super::layout::TableId;
use super::node::ChildRef;

const FORMAT: u8 = 1;

/// The root page of one table's tree, its height, and its entry count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Root {
    pub(crate) page: ChildRef,
    pub(crate) level: u8,
    pub(crate) entries: u64,
}

/// What a header's catalog pointer reaches: every table's root and the extent table.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Catalog {
    pub(crate) roots: BTreeMap<TableId, Root>,
    pub(crate) extents: ExtentTable,
}

impl Catalog {
    pub(crate) fn encode(&self) -> Result<Vec<u8>, EngineError> {
        let mut out = vec![FORMAT];
        put_u64(&mut out, self.extents.next_slot);
        put_u64(&mut out, self.extents.next_extent.0);
        for open in self.extents.open {
            put_u64(&mut out, open.map_or(0, |id| id.0));
        }
        put_u32(
            &mut out,
            u32::try_from(self.roots.len()).map_err(|_| EngineError::TooLarge)?,
        );
        for (table, root) in &self.roots {
            put_u16(&mut out, table.0);
            put_u64(&mut out, root.page.at.raw());
            put_u64(&mut out, root.page.lsn);
            out.push(root.level);
            put_u64(&mut out, root.entries);
        }
        put_u32(
            &mut out,
            u32::try_from(self.extents.entries.len()).map_err(|_| EngineError::TooLarge)?,
        );
        for (id, entry) in &self.extents.entries {
            put_u64(&mut out, id.0);
            put_u64(&mut out, entry.slot);
            put_u32(&mut out, entry.written);
            put_u32(&mut out, entry.live);
            out.push(match entry.state {
                ExtentState::Open => 0,
                ExtentState::Sealed => 1,
            });
            out.push(entry.purpose.code());
            out.push(entry.scheme);
            out.push(0);
            put_u64(&mut out, entry.opened_term);
            put_u64(&mut out, 0);
        }
        Ok(out)
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, EngineError> {
        let mut reader = Reader::new(bytes, "the catalog");
        let format = reader.u8()?;
        if format != FORMAT {
            return Err(EngineError::Unsupported(format!("catalog format {format}")));
        }
        let next_slot = reader.u64()?;
        let next_extent = ExtentId(reader.u64()?);
        let mut open = [None; OPEN_EXTENTS];
        for entry in &mut open {
            let id = reader.u64()?;
            *entry = (id != 0).then_some(ExtentId(id));
        }
        let mut roots = BTreeMap::new();
        for _ in 0..reader.u32()? {
            let table = TableId(reader.u16()?);
            let at = BlockRef::from_raw(reader.u64()?)?;
            let lsn = reader.u64()?;
            let level = reader.u8()?;
            let entries = reader.u64()?;
            table.table()?;
            if at.is_null() || entries == 0 {
                return Err(reader.bad("table root"));
            }
            let root = Root {
                page: ChildRef { at, lsn },
                level,
                entries,
            };
            if roots.insert(table, root).is_some() {
                return Err(reader.bad("table list"));
            }
        }
        let mut entries = BTreeMap::new();
        let mut previous = ExtentId::NONE;
        for _ in 0..reader.u32()? {
            let id = ExtentId(reader.u64()?);
            let slot = reader.u64()?;
            let written = reader.u32()?;
            let live = reader.u32()?;
            let state = match reader.u8()? {
                0 => ExtentState::Open,
                1 => ExtentState::Sealed,
                _ => return Err(reader.bad("extent state")),
            };
            let purpose = Purpose::from_code(reader.u8()?).ok_or_else(|| reader.bad("purpose"))?;
            let scheme = reader.u8()?;
            let reserved = reader.u8()?;
            let opened_term = reader.u64()?;
            let tail = reader.u64()?;
            if id <= previous || reserved != 0 || tail != 0 || scheme != 0 {
                return Err(reader.bad("extent entry"));
            }
            previous = id;
            entries.insert(
                id,
                ExtentEntry {
                    slot,
                    written,
                    live,
                    state,
                    purpose,
                    scheme,
                    opened_term,
                },
            );
        }
        reader.finish()?;
        Ok(Self {
            roots,
            extents: ExtentTable {
                entries,
                next_extent,
                next_slot,
                open,
            },
        })
    }
}

#[cfg(test)]
#[path = "catalog.test.rs"]
mod tests;
