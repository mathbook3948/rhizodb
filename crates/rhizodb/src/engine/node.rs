use std::cmp::Ordering;

use super::blob::BlobRef;
use super::blob::RUN_PAYLOAD_LIMIT;
use super::blob::RunRef;
use super::blob::run_blocks;
use super::codec::Reader;
use super::codec::put_u16;
use super::codec::put_u32;
use super::codec::put_u64;
use super::error::EngineError;
use super::layout::BLOCK;
use super::layout::BlockRef;
use super::layout::TableId;
use super::memtable::Bytes;
use super::page::PAGE_HEADER;
use super::page::PageHeader;
use super::page::PageKind;

/// Keys longer than this live in a run of their own, and their cell keeps the first bytes.
pub(crate) const INLINE_KEY: usize = 1024;

/// Values longer than this live in a run of their own.
pub(crate) const INLINE_VALUE: usize = 1024;

/// The bytes of a long key after the page prefix that its cell keeps, for comparisons.
pub(crate) const STUB: usize = 64;

/// The longest prefix a page shares among its keys.
pub(crate) const MAX_PREFIX: usize = 512;

const VARIABLE: u8 = 1;
const FIXED: u8 = 2;

const EXTERNAL_KEY: u16 = 0x8000;
const BLOB_REF: usize = 36;
const LONG_KEY_CELL: usize = 2 + 4 + STUB + BLOB_REF;
const VALUE_REF: usize = 1 + BLOB_REF;

/// A pointer from a branch to a child page, with the LSN the child must carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ChildRef {
    pub(crate) at: BlockRef,
    pub(crate) lsn: u64,
}

/// A leaf's value: in the cell, or in a run the cell points to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum StoredValue {
    Inline(Bytes),
    External(BlobRef),
}

impl StoredValue {
    fn cell_bytes(&self) -> usize {
        match self {
            StoredValue::Inline(value) => 1 + 2 + value.len(),
            StoredValue::External(_) => VALUE_REF,
        }
    }
}

/// The full key and value of one leaf cell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LeafEntry {
    pub(crate) key: Bytes,
    pub(crate) value: StoredValue,
}

/// One child of a branch with the lowest key it may hold. The first child's low key is the
/// branch's own and is not stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BranchEntry {
    pub(crate) low: Bytes,
    pub(crate) child: ChildRef,
}

/// The cells of one page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Content {
    Leaf(Vec<LeafEntry>),
    Branch(Vec<BranchEntry>),
}

impl Content {
    pub(crate) fn len(&self) -> usize {
        match self {
            Content::Leaf(entries) => entries.len(),
            Content::Branch(entries) => entries.len(),
        }
    }
}

fn common_prefix(first: &[u8], last: &[u8]) -> usize {
    first
        .iter()
        .zip(last)
        .take_while(|(a, b)| a == b)
        .count()
        .min(MAX_PREFIX)
}

/// The size of a page as cells are added in key order, kept in constant time per cell.
#[derive(Clone, Debug, Default)]
pub(crate) struct PageSize {
    first_key: Option<Bytes>,
    prefix: usize,
    cells: usize,
    inline_keys: usize,
    bytes: usize,
}

impl PageSize {
    /// The page with one more cell, whose key is stored unless it is `None`, and whose other
    /// parts take `rest` bytes, and the bytes that page takes.
    pub(crate) fn with(&self, key: Option<&Bytes>, rest: usize) -> (PageSize, usize) {
        let mut next = self.clone();
        next.cells += 1;
        next.bytes += 2 + rest;
        if let Some(key) = key {
            match &self.first_key {
                None => {
                    next.first_key = Some(key.clone());
                    next.prefix = key.len().min(MAX_PREFIX);
                }
                Some(first) => next.prefix = common_prefix(first, key),
            }
            if key.len() > INLINE_KEY {
                next.bytes += LONG_KEY_CELL;
            } else {
                next.inline_keys += 1;
                next.bytes += 2 + key.len();
            }
        }
        let size = PAGE_HEADER + 2 + next.prefix + next.bytes - next.inline_keys * next.prefix;
        (next, size)
    }
}

/// The bytes of a branch cell other than its key.
pub(crate) const CHILD_BYTES: usize = 16;

/// The bytes of a leaf cell's value part.
pub(crate) fn value_bytes(value: &StoredValue) -> usize {
    value.cell_bytes()
}

/// Writes a page's body. `long_key` stores a key longer than [`INLINE_KEY`] in a run and
/// returns where.
pub(crate) fn encode(
    content: &Content,
    mut long_key: impl FnMut(&[u8]) -> Result<BlobRef, EngineError>,
) -> Result<(u8, u16, Vec<u8>), EngineError> {
    match content {
        Content::Leaf(entries) => {
            if let Some(fixed) = encode_fixed(entries) {
                return Ok((FIXED, entries.len() as u16, fixed));
            }
            let keys: Vec<&[u8]> = entries.iter().map(|entry| &*entry.key).collect();
            encode_variable(&keys, 0, &mut long_key, |out, index| {
                encode_value(out, &entries[index].value)
            })
            .map(|(cells, body)| (VARIABLE, cells, body))
        }
        Content::Branch(entries) => {
            let keys: Vec<&[u8]> = entries.iter().map(|entry| &*entry.low).collect();
            encode_variable(&keys, 1, &mut long_key, |out, index| {
                let child = entries[index].child;
                put_u64(out, child.at.raw());
                put_u64(out, child.lsn);
                Ok(())
            })
            .map(|(cells, body)| (VARIABLE, cells, body))
        }
    }
}

fn encode_value(out: &mut Vec<u8>, value: &StoredValue) -> Result<(), EngineError> {
    match value {
        StoredValue::Inline(bytes) => {
            if bytes.len() > INLINE_VALUE {
                return Err(EngineError::TooLarge);
            }
            out.push(0);
            put_u16(out, bytes.len() as u16);
            out.extend_from_slice(bytes);
        }
        StoredValue::External(blob) => {
            out.push(1);
            put_blob(out, blob);
        }
    }
    Ok(())
}

fn put_blob(out: &mut Vec<u8>, blob: &BlobRef) {
    put_u64(out, blob.first.at.raw());
    put_u32(out, blob.first.blocks);
    put_u64(out, blob.first.lsn);
    put_u64(out, blob.length);
    put_u64(out, blob.checksum);
}

fn read_blob(reader: &mut Reader<'_>) -> Result<BlobRef, EngineError> {
    let blob = BlobRef {
        first: RunRef {
            at: BlockRef::from_raw(reader.u64()?)?,
            blocks: reader.u32()?,
            lsn: reader.u64()?,
        },
        length: reader.u64()?,
        checksum: reader.u64()?,
    };
    let first_payload = usize::try_from(blob.length)
        .unwrap_or(usize::MAX)
        .min(RUN_PAYLOAD_LIMIT);
    if blob.first.at.is_null() || blob.first.blocks != run_blocks(first_payload) {
        return Err(reader.bad("run pointer"));
    }
    Ok(blob)
}

fn encode_fixed(entries: &[LeafEntry]) -> Option<Vec<u8>> {
    let first = entries.first()?;
    let last = entries.last()?;
    let prefix = common_prefix(&first.key, &last.key);
    let suffix = first.key.len() - prefix;
    let StoredValue::Inline(first_value) = &first.value else {
        return None;
    };
    let width = first_value.len();
    let same = entries.iter().all(|entry| {
        entry.key.len() == first.key.len()
            && entry.key.len() <= INLINE_KEY
            && matches!(&entry.value, StoredValue::Inline(value) if value.len() == width)
    });
    if !same {
        return None;
    }
    let mut out = Vec::new();
    put_u16(&mut out, prefix as u16);
    out.extend_from_slice(&first.key[..prefix]);
    put_u16(&mut out, suffix as u16);
    put_u16(&mut out, width as u16);
    for entry in entries {
        out.extend_from_slice(&entry.key[prefix..]);
        if let StoredValue::Inline(value) = &entry.value {
            out.extend_from_slice(value);
        }
    }
    (PAGE_HEADER + out.len() <= BLOCK).then_some(out)
}

fn encode_variable(
    keys: &[&[u8]],
    unkeyed: usize,
    long_key: &mut impl FnMut(&[u8]) -> Result<BlobRef, EngineError>,
    mut rest: impl FnMut(&mut Vec<u8>, usize) -> Result<(), EngineError>,
) -> Result<(u16, Vec<u8>), EngineError> {
    let keyed = keys.get(unkeyed..).unwrap_or_default();
    let prefix = match (keyed.first(), keyed.last()) {
        (Some(first), Some(last)) => common_prefix(first, last),
        _ => 0,
    };
    let prefix_bytes = keyed.first().map_or(&[][..], |first| &first[..prefix]);
    let cells = u16::try_from(keys.len()).map_err(|_| EngineError::TooLarge)?;
    let mut out = Vec::with_capacity(BLOCK);
    put_u16(&mut out, prefix as u16);
    out.extend_from_slice(prefix_bytes);
    let offsets_at = out.len();
    out.resize(offsets_at + 2 * keys.len(), 0);
    for (index, key) in keys.iter().enumerate() {
        let offset = PAGE_HEADER + out.len();
        let offset = u16::try_from(offset).map_err(|_| EngineError::TooLarge)?;
        out[offsets_at + 2 * index..offsets_at + 2 * index + 2]
            .copy_from_slice(&offset.to_le_bytes());
        rest(&mut out, index)?;
        if index < unkeyed {
            continue;
        }
        if key.len() > INLINE_KEY {
            let blob = long_key(key)?;
            put_u16(&mut out, EXTERNAL_KEY);
            put_u32(
                &mut out,
                u32::try_from(key.len()).map_err(|_| EngineError::TooLarge)?,
            );
            let stub_end = key.len().min(prefix + STUB);
            let stub = &key[prefix..stub_end];
            out.extend_from_slice(stub);
            out.resize(out.len() + STUB - stub.len(), 0);
            put_blob(&mut out, &blob);
        } else {
            put_u16(&mut out, (key.len() - prefix) as u16);
            out.extend_from_slice(&key[prefix..]);
        }
    }
    if PAGE_HEADER + out.len() > BLOCK {
        return Err(EngineError::corrupt("a page was packed past its block"));
    }
    Ok((cells, out))
}

/// The key of one cell, without loading a long key's run.
#[derive(Clone, Copy, Debug)]
pub(crate) enum CellKey<'a> {
    Inline {
        prefix: &'a [u8],
        suffix: &'a [u8],
    },
    Long {
        prefix: &'a [u8],
        stub: &'a [u8],
        length: u32,
        blob: BlobRef,
    },
}

impl CellKey<'_> {
    /// Compares the cell's key with `target`, loading a long key only when its first bytes tie.
    pub(crate) fn compare(
        &self,
        target: &[u8],
        load: &dyn Fn(&BlobRef) -> Result<Vec<u8>, EngineError>,
    ) -> Result<Ordering, EngineError> {
        match self {
            CellKey::Inline { prefix, suffix } => Ok(compare_parts(prefix, suffix, target)),
            CellKey::Long {
                prefix, stub, blob, ..
            } => match compare_parts(prefix, stub, target) {
                Ordering::Equal | Ordering::Less
                    if target.len() >= prefix.len() + stub.len()
                        && target.starts_with(prefix)
                        && target[prefix.len()..].starts_with(stub) =>
                {
                    Ok(load(blob)?.as_slice().cmp(target))
                }
                order => Ok(order),
            },
        }
    }

    /// The whole key, loading a long key's run.
    pub(crate) fn full(
        &self,
        load: &dyn Fn(&BlobRef) -> Result<Vec<u8>, EngineError>,
    ) -> Result<Vec<u8>, EngineError> {
        match self {
            CellKey::Inline { prefix, suffix } => Ok([*prefix, *suffix].concat()),
            CellKey::Long {
                prefix,
                stub,
                length,
                blob,
            } => {
                let key = load(blob)?;
                if key.len() != *length as usize
                    || !key.starts_with(prefix)
                    || !key[prefix.len()..].starts_with(stub)
                {
                    return Err(EngineError::corrupt(
                        "a long key's run does not match its cell",
                    ));
                }
                Ok(key)
            }
        }
    }

    pub(crate) fn blob(&self) -> Option<&BlobRef> {
        match self {
            CellKey::Long { blob, .. } => Some(blob),
            CellKey::Inline { .. } => None,
        }
    }
}

fn compare_parts(prefix: &[u8], suffix: &[u8], target: &[u8]) -> Ordering {
    let split = prefix.len().min(target.len());
    prefix.cmp(&target[..split]).then_with(|| {
        if target.len() < prefix.len() {
            Ordering::Greater
        } else {
            suffix.cmp(&target[prefix.len()..])
        }
    })
}

/// A cell's value as the page holds it.
#[derive(Clone, Copy, Debug)]
pub(crate) enum CellValue<'a> {
    Inline(&'a [u8]),
    External(BlobRef),
}

/// A checked page read in place: cells are decoded only when asked for.
#[derive(Clone, Debug)]
pub(crate) struct Node<'a> {
    pub(crate) header: PageHeader,
    page: &'a [u8],
    prefix: &'a [u8],
    layout: Layout<'a>,
}

#[derive(Clone, Copy, Debug)]
enum Layout<'a> {
    Variable {
        offsets: &'a [u8],
    },
    Fixed {
        suffix: usize,
        width: usize,
        cells: &'a [u8],
    },
}

fn bad(header: &PageHeader) -> EngineError {
    EngineError::corrupt(format!("the page at {:?} has an invalid cell", header.at))
}

impl<'a> Node<'a> {
    /// Reads the layout of a page whose header was checked, and fails as corruption when the
    /// layout does not fit in it.
    pub(crate) fn parse(
        header: PageHeader,
        page: &'a [u8],
        table: TableId,
    ) -> Result<Self, EngineError> {
        if page.len() != BLOCK || header.table != table {
            return Err(EngineError::corrupt(format!(
                "the page at {:?} belongs to table {:#x}, not {:#x}",
                header.at, header.table.0, table.0
            )));
        }
        let leaf = match (header.kind, header.level) {
            (PageKind::Leaf, 0) => true,
            (PageKind::Branch, level) if level > 0 => false,
            _ => return Err(bad(&header)),
        };
        let mut reader = Reader::new(&page[PAGE_HEADER..], "a page body");
        let prefix_length = reader.u16()? as usize;
        let cells = usize::from(header.cells);
        if prefix_length > MAX_PREFIX || cells == 0 || header.flags != 0 {
            return Err(bad(&header));
        }
        let prefix = reader.take(prefix_length)?;
        let layout = match header.format {
            VARIABLE => Layout::Variable {
                offsets: reader.take(2 * cells)?,
            },
            FIXED if leaf => {
                let suffix = reader.u16()? as usize;
                let width = reader.u16()? as usize;
                let total = cells
                    .checked_mul(suffix + width)
                    .ok_or_else(|| bad(&header))?;
                if suffix + prefix_length > INLINE_KEY || width > INLINE_VALUE {
                    return Err(bad(&header));
                }
                Layout::Fixed {
                    suffix,
                    width,
                    cells: reader.take(total)?,
                }
            }
            _ => return Err(bad(&header)),
        };
        Ok(Self {
            header,
            page,
            prefix,
            layout,
        })
    }

    pub(crate) fn cells(&self) -> usize {
        usize::from(self.header.cells)
    }

    pub(crate) fn is_leaf(&self) -> bool {
        self.header.level == 0
    }

    fn cell(&self, index: usize) -> Result<Reader<'a>, EngineError> {
        let Layout::Variable { offsets } = self.layout else {
            return Err(bad(&self.header));
        };
        let offset = offsets
            .get(2 * index..2 * index + 2)
            .map(|bytes| usize::from(u16::from_le_bytes([bytes[0], bytes[1]])))
            .ok_or_else(|| bad(&self.header))?;
        let cell = self
            .page
            .get(offset..)
            .filter(|_| offset >= PAGE_HEADER)
            .ok_or_else(|| bad(&self.header))?;
        Ok(Reader::new(cell, "a page cell"))
    }

    fn read_key(&self, reader: &mut Reader<'a>) -> Result<CellKey<'a>, EngineError> {
        let word = reader.u16()?;
        if word & EXTERNAL_KEY == 0 {
            let suffix = reader.take(usize::from(word))?;
            if self.prefix.len() + suffix.len() > INLINE_KEY {
                return Err(bad(&self.header));
            }
            return Ok(CellKey::Inline {
                prefix: self.prefix,
                suffix,
            });
        }
        if word != EXTERNAL_KEY {
            return Err(bad(&self.header));
        }
        let length = reader.u32()?;
        let stored = reader.take(STUB)?;
        let stub_length = (length as usize)
            .checked_sub(self.prefix.len())
            .filter(|_| length as usize > INLINE_KEY)
            .ok_or_else(|| bad(&self.header))?
            .min(STUB);
        if stored[stub_length..].iter().any(|byte| *byte != 0) {
            return Err(bad(&self.header));
        }
        let blob = read_blob(reader)?;
        if blob.length != u64::from(length) {
            return Err(bad(&self.header));
        }
        Ok(CellKey::Long {
            prefix: self.prefix,
            stub: &stored[..stub_length],
            length,
            blob,
        })
    }

    /// The key of cell `index`. The first cell of a branch has none.
    pub(crate) fn key(&self, index: usize) -> Result<CellKey<'a>, EngineError> {
        match self.layout {
            Layout::Fixed {
                suffix,
                width,
                cells,
            } => {
                let start = index * (suffix + width);
                let suffix = cells
                    .get(start..start + suffix)
                    .ok_or_else(|| bad(&self.header))?;
                Ok(CellKey::Inline {
                    prefix: self.prefix,
                    suffix,
                })
            }
            Layout::Variable { .. } => {
                if !self.is_leaf() && index == 0 {
                    return Err(bad(&self.header));
                }
                let mut reader = self.cell(index)?;
                if self.is_leaf() {
                    let tag = reader.u8()?;
                    match tag {
                        0 => {
                            let length = usize::from(reader.u16()?);
                            reader.take(length)?;
                        }
                        1 => {
                            read_blob(&mut reader)?;
                        }
                        _ => return Err(bad(&self.header)),
                    }
                } else {
                    reader.take(CHILD_BYTES)?;
                }
                self.read_key(&mut reader)
            }
        }
    }

    /// The value of leaf cell `index`.
    pub(crate) fn value(&self, index: usize) -> Result<CellValue<'a>, EngineError> {
        match self.layout {
            Layout::Fixed {
                suffix,
                width,
                cells,
            } => {
                let start = index * (suffix + width) + suffix;
                cells
                    .get(start..start + width)
                    .map(CellValue::Inline)
                    .ok_or_else(|| bad(&self.header))
            }
            Layout::Variable { .. } => {
                if !self.is_leaf() {
                    return Err(bad(&self.header));
                }
                let mut reader = self.cell(index)?;
                match reader.u8()? {
                    0 => {
                        let length = usize::from(reader.u16()?);
                        if length > INLINE_VALUE {
                            return Err(bad(&self.header));
                        }
                        Ok(CellValue::Inline(reader.take(length)?))
                    }
                    1 => {
                        let blob = read_blob(&mut reader)?;
                        if blob.length <= INLINE_VALUE as u64 {
                            return Err(bad(&self.header));
                        }
                        Ok(CellValue::External(blob))
                    }
                    _ => Err(bad(&self.header)),
                }
            }
        }
    }

    /// The child of branch cell `index`.
    pub(crate) fn child(&self, index: usize) -> Result<ChildRef, EngineError> {
        if self.is_leaf() || index >= self.cells() {
            return Err(bad(&self.header));
        }
        let mut reader = self.cell(index)?;
        let at = BlockRef::from_raw(reader.u64()?)?;
        let lsn = reader.u64()?;
        if at.is_null() {
            return Err(bad(&self.header));
        }
        Ok(ChildRef { at, lsn })
    }

    /// The first cell whose key is at least `target` in a leaf, or in a branch the child whose
    /// range holds `target`.
    pub(crate) fn search(
        &self,
        target: &[u8],
        load: &dyn Fn(&BlobRef) -> Result<Vec<u8>, EngineError>,
    ) -> Result<usize, EngineError> {
        let (mut low, mut high) = if self.is_leaf() {
            (0, self.cells())
        } else {
            (1, self.cells())
        };
        while low < high {
            let middle = low + (high - low) / 2;
            if self.key(middle)?.compare(target, load)? == Ordering::Less {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        if self.is_leaf() {
            return Ok(low);
        }
        if low < self.cells() && self.key(low)?.compare(target, load)? == Ordering::Equal {
            return Ok(low);
        }
        Ok(low - 1)
    }

    /// Every cell, with long keys loaded, and the runs of the long keys this page owns.
    pub(crate) fn content(
        &self,
        load: &dyn Fn(&BlobRef) -> Result<Vec<u8>, EngineError>,
        low: &Bytes,
    ) -> Result<(Content, Vec<BlobRef>), EngineError> {
        let mut owned = Vec::new();
        let mut previous: Option<Vec<u8>> = None;
        let mut check_order = |key: &[u8]| -> Result<(), EngineError> {
            if previous.as_deref().is_some_and(|before| before >= key) {
                return Err(bad(&self.header));
            }
            previous = Some(key.to_vec());
            Ok(())
        };
        if self.is_leaf() {
            let mut entries = Vec::with_capacity(self.cells());
            for index in 0..self.cells() {
                let key = self.key(index)?;
                owned.extend(key.blob().copied());
                let key = key.full(load)?;
                check_order(&key)?;
                let value = match self.value(index)? {
                    CellValue::Inline(value) => StoredValue::Inline(Bytes::from(value)),
                    CellValue::External(blob) => StoredValue::External(blob),
                };
                entries.push(LeafEntry {
                    key: Bytes::from(key),
                    value,
                });
            }
            Ok((Content::Leaf(entries), owned))
        } else {
            let mut entries = Vec::with_capacity(self.cells());
            for index in 0..self.cells() {
                let low = if index == 0 {
                    low.clone()
                } else {
                    let key = self.key(index)?;
                    owned.extend(key.blob().copied());
                    let key = key.full(load)?;
                    check_order(&key)?;
                    Bytes::from(key)
                };
                entries.push(BranchEntry {
                    low,
                    child: self.child(index)?,
                });
            }
            Ok((Content::Branch(entries), owned))
        }
    }
}

#[cfg(test)]
#[path = "node.test.rs"]
mod tests;
