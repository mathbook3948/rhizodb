use std::collections::BTreeMap;
use std::sync::Arc;

use super::blob::BlobRef;
use super::blob::RUN_PAYLOAD_LIMIT;
use super::blob::Role;
use super::blob::RunRef;
use super::blob::RunSpec;
use super::blob::decode_run;
use super::blob::item_checksum;
use super::blob::run_blocks;
use super::blob::split_runs;
use super::block_file::BlockFile;
use super::catalog::Catalog;
use super::catalog::Root;
use super::error::EngineError;
use super::extent::ExtentTable;
use super::extent::Purpose;
use super::layout::BLOCK;
use super::layout::ExtentId;
use super::layout::TableId;
use super::memtable::Bytes;
use super::memtable::Slot;
use super::node::BranchEntry;
use super::node::CHILD_BYTES;
use super::node::ChildRef;
use super::node::Content;
use super::node::INLINE_VALUE;
use super::node::LeafEntry;
use super::node::Node;
use super::node::PageSize;
use super::node::StoredValue;
use super::node::encode;
use super::node::value_bytes;
use super::page::PAGE_HEADER;
use super::page::PageHeader;
use super::page::PageKind;
use super::pages::PageSource;
use super::pages::Pages;
use super::pages::load_blob;
use super::tree::MAX_LEVEL;
use super::tree::Tree;
use super::tree::fetch;
use crate::storage::StorageError;
use std::ops::Bound;

const UNDERFULL: usize = BLOCK / 4;

#[derive(Debug)]
enum Child {
    Existing { low: Bytes, page: ChildRef },
    Fresh { low: Bytes, content: Content },
}

impl Child {
    fn low(&self) -> &Bytes {
        match self {
            Child::Existing { low, .. } | Child::Fresh { low, .. } => low,
        }
    }
}

/// The shortest key above `before` and at most `after`, which separates two neighbouring pages.
pub(crate) fn separator(before: &[u8], after: &[u8]) -> Bytes {
    let shared = before.iter().zip(after).take_while(|(a, b)| a == b).count();
    Bytes::from(&after[..(shared + 1).min(after.len())])
}

fn cell_rest(content: &Content, index: usize) -> (Option<&Bytes>, usize) {
    match content {
        Content::Leaf(entries) => (
            Some(&entries[index].key),
            value_bytes(&entries[index].value),
        ),
        Content::Branch(entries) => ((index > 0).then_some(&entries[index].low), CHILD_BYTES),
    }
}

fn content_bytes(content: &Content) -> usize {
    let mut size = PageSize::default();
    let mut bytes = PAGE_HEADER;
    for index in 0..content.len() {
        let (key, rest) = cell_rest(content, index);
        (size, bytes) = size.with(key, rest);
    }
    bytes
}

fn greedy(content: Content, limit: usize) -> Vec<Content> {
    let mut pieces = Vec::new();
    match content {
        Content::Leaf(entries) => {
            let mut current = Vec::new();
            let mut size = PageSize::default();
            for entry in entries {
                let (next, bytes) = size.with(Some(&entry.key), value_bytes(&entry.value));
                if !current.is_empty() && bytes > limit {
                    pieces.push(Content::Leaf(std::mem::take(&mut current)));
                    size = PageSize::default()
                        .with(Some(&entry.key), value_bytes(&entry.value))
                        .0;
                } else {
                    size = next;
                }
                current.push(entry);
            }
            if !current.is_empty() {
                pieces.push(Content::Leaf(current));
            }
        }
        Content::Branch(entries) => {
            let mut current: Vec<BranchEntry> = Vec::new();
            let mut size = PageSize::default();
            for entry in entries {
                let key = (!current.is_empty()).then_some(&entry.low);
                let (next, bytes) = size.with(key, CHILD_BYTES);
                if !current.is_empty() && bytes > limit {
                    pieces.push(Content::Branch(std::mem::take(&mut current)));
                    size = PageSize::default().with(None, CHILD_BYTES).0;
                } else {
                    size = next;
                }
                current.push(entry);
            }
            if !current.is_empty() {
                pieces.push(Content::Branch(current));
            }
        }
    }
    pieces
}

fn pack(content: Content, rightmost: bool) -> Vec<Content> {
    let pieces = greedy(content, BLOCK);
    if rightmost || pieces.len() < 2 {
        return pieces;
    }
    let count = pieces.len();
    let total: usize = pieces.iter().map(content_bytes).sum();
    let limit = (total / count + total / count / 8).min(BLOCK);
    let joined = join(pieces);
    let even = greedy(joined.clone(), limit);
    if even.len() <= count {
        even
    } else {
        greedy(joined, BLOCK)
    }
}

fn join(pieces: Vec<Content>) -> Content {
    let mut leaves = Vec::new();
    let mut branches = Vec::new();
    for piece in pieces {
        match piece {
            Content::Leaf(entries) => leaves.extend(entries),
            Content::Branch(entries) => branches.extend(entries),
        }
    }
    if branches.is_empty() {
        Content::Leaf(leaves)
    } else {
        Content::Branch(branches)
    }
}

fn lows(low: Bytes, pieces: Vec<Content>) -> Vec<Child> {
    let mut children = Vec::with_capacity(pieces.len());
    let mut previous_last: Option<Bytes> = None;
    for content in pieces {
        let piece_low = match (&previous_last, &content) {
            (None, _) => low.clone(),
            (Some(last), Content::Leaf(entries)) => match entries.first() {
                Some(first) => separator(last, &first.key),
                None => low.clone(),
            },
            (Some(_), Content::Branch(entries)) => entries
                .first()
                .map_or_else(|| low.clone(), |first| first.low.clone()),
        };
        previous_last = match &content {
            Content::Leaf(entries) => entries.last().map(|entry| entry.key.clone()),
            Content::Branch(entries) => entries.last().map(|entry| entry.low.clone()),
        };
        let content = match content {
            Content::Branch(mut entries) => {
                if let Some(first) = entries.first_mut() {
                    first.low = piece_low.clone();
                }
                Content::Branch(entries)
            }
            leaf => leaf,
        };
        children.push(Child::Fresh {
            low: piece_low,
            content,
        });
    }
    children
}

/// Writes one checkpoint's pages: the changed parts of each table's tree, the runs of long keys
/// and values, and the catalog. It works on its own copy of the extent table, which becomes the
/// engine's only once the checkpoint's header is durable.
#[derive(Debug)]
pub(crate) struct Checkpointer<'a, B: BlockFile> {
    pages: &'a Pages<B>,
    pub(crate) extents: ExtentTable,
    lsn: u64,
    generation: u64,
    entry_change: i64,
}

impl<'a, B: BlockFile> Checkpointer<'a, B> {
    pub(crate) fn new(
        pages: &'a Pages<B>,
        extents: ExtentTable,
        lsn: u64,
        generation: u64,
    ) -> Self {
        Self {
            pages,
            extents,
            lsn,
            generation,
            entry_change: 0,
        }
    }

    fn source(&self) -> &dyn PageSource {
        self.pages
    }

    fn plan_runs(
        extents: &mut ExtentTable,
        pages: &Pages<B>,
        purpose: Purpose,
        length: usize,
        lsn: u64,
    ) -> Result<Vec<RunRef>, EngineError> {
        let mut runs = split_runs(length)
            .into_iter()
            .rev()
            .map(|payload| {
                let blocks = run_blocks(payload);
                let at = extents.place(purpose, blocks, |id, slot| pages.place_extent(id, slot))?;
                Ok(RunRef { at, blocks, lsn })
            })
            .collect::<Result<Vec<_>, EngineError>>()?;
        runs.reverse();
        Ok(runs)
    }

    fn write_runs(
        &self,
        runs: &[RunRef],
        role: Role,
        table: TableId,
        owner: &[u8],
        bytes: &[u8],
    ) -> Result<BlobRef, EngineError> {
        let sizes = split_runs(bytes.len());
        let mut offset = 0;
        for (index, (run, size)) in runs.iter().zip(&sizes).enumerate() {
            let next = runs.get(index + 1).copied().unwrap_or_default();
            let next_slot = if next.at.is_null() {
                0
            } else {
                self.extents
                    .entries
                    .get(&next.at.extent())
                    .map(|entry| entry.slot)
                    .ok_or_else(|| {
                        EngineError::corrupt("a run was planned outside the extent table")
                    })?
            };
            let encoded = RunSpec {
                role,
                table,
                at: run.at,
                lsn: self.lsn,
                generation: self.generation,
                total: bytes.len() as u64,
                next,
                next_slot,
                owner,
                payload: &bytes[offset..offset + size],
            }
            .encode(self.pages.seed())?;
            self.pages.write(run.at, &encoded)?;
            offset += size;
        }
        let first = runs
            .first()
            .copied()
            .ok_or_else(|| EngineError::corrupt("an item was planned with no run"))?;
        Ok(BlobRef {
            first,
            length: bytes.len() as u64,
            checksum: item_checksum(bytes),
        })
    }

    fn write_item(
        &mut self,
        role: Role,
        table: TableId,
        owner: &[u8],
        bytes: &[u8],
    ) -> Result<BlobRef, EngineError> {
        let runs = Self::plan_runs(
            &mut self.extents,
            self.pages,
            Purpose::Values,
            bytes.len(),
            self.lsn,
        )?;
        self.write_runs(&runs, role, table, owner, bytes)
    }

    fn free_item(&mut self, blob: &BlobRef, role: Role) -> Result<(), EngineError> {
        let mut run = blob.first;
        for _ in 0..(blob.length as usize / RUN_PAYLOAD_LIMIT + 1) {
            self.extents.free(run.at, run.blocks)?;
            if blob.length as usize <= RUN_PAYLOAD_LIMIT {
                return Ok(());
            }
            let bytes = self.source().run_bytes(run)?;
            let (_, decoded) = decode_run(self.pages.seed(), &bytes, run, role)?;
            if decoded.next.at.is_null() {
                return Ok(());
            }
            run = decoded.next;
        }
        Err(EngineError::corrupt(
            "an item has more runs than its length allows",
        ))
    }

    fn write_node(
        &mut self,
        table: TableId,
        level: u8,
        content: &Content,
    ) -> Result<ChildRef, EngineError> {
        let mut long_keys = Vec::new();
        let (format, cells, body) = encode(content, |key| {
            long_keys.push(key.to_vec());
            Ok(BlobRef::default())
        })?;
        let mut blobs = Vec::with_capacity(long_keys.len());
        for key in &long_keys {
            blobs.push(self.write_item(Role::Key, table, key, key)?);
        }
        let mut written = blobs.into_iter();
        let (format, cells, body) = if long_keys.is_empty() {
            (format, cells, body)
        } else {
            encode(content, |_| {
                written
                    .next()
                    .ok_or_else(|| EngineError::corrupt("a long key was not written"))
            })?
        };
        let at = self.extents.place(Purpose::Tree, 1, |id, slot| {
            self.pages.place_extent(id, slot)
        })?;
        let mut page = vec![0; PAGE_HEADER];
        page.extend_from_slice(&body);
        let free = page.len() as u16;
        page.resize(BLOCK, 0);
        PageHeader {
            kind: if level == 0 {
                PageKind::Leaf
            } else {
                PageKind::Branch
            },
            format,
            table,
            level,
            flags: 0,
            cells,
            free,
            at,
            lsn: self.lsn,
            generation: self.generation,
        }
        .seal(self.pages.seed(), &mut page)?;
        self.pages.write_page(at, page)?;
        Ok(ChildRef { at, lsn: self.lsn })
    }

    fn take_node(
        &mut self,
        table: TableId,
        page: ChildRef,
        level: u8,
        low: &Bytes,
    ) -> Result<Content, EngineError> {
        let cached = fetch(self.source(), table, page, level)?;
        let node = Node::parse(&cached.header, &cached.bytes, table)?;
        let load = |blob: &BlobRef| load_blob(self.source(), blob, Role::Key);
        let (content, owned) = node.content(&load, low)?;
        self.extents.free(page.at, 1)?;
        for blob in owned {
            self.free_item(&blob, Role::Key)?;
        }
        Ok(content)
    }

    fn stored(
        &mut self,
        table: TableId,
        key: &[u8],
        value: &Bytes,
    ) -> Result<StoredValue, EngineError> {
        if value.len() <= INLINE_VALUE {
            return Ok(StoredValue::Inline(value.clone()));
        }
        Ok(StoredValue::External(self.write_item(
            Role::Value,
            table,
            key,
            value,
        )?))
    }

    fn release_value(&mut self, value: &StoredValue) -> Result<(), EngineError> {
        if let StoredValue::External(blob) = value {
            self.free_item(blob, Role::Value)?;
        }
        Ok(())
    }

    fn merge_leaf(
        &mut self,
        table: TableId,
        old: Vec<LeafEntry>,
        changes: &[(Bytes, Slot)],
    ) -> Result<Vec<LeafEntry>, EngineError> {
        let mut merged = Vec::with_capacity(old.len() + changes.len());
        let mut old = old.into_iter().peekable();
        for (key, slot) in changes {
            while let Some(entry) = old.next_if(|entry| entry.key < *key) {
                merged.push(entry);
            }
            let existed = match old.next_if(|entry| entry.key == *key) {
                Some(entry) => {
                    self.release_value(&entry.value)?;
                    true
                }
                None => false,
            };
            match slot {
                Some(value) => {
                    let value = self.stored(table, key, value)?;
                    merged.push(LeafEntry {
                        key: key.clone(),
                        value,
                    });
                    if !existed {
                        self.entry_change += 1;
                    }
                }
                None => {
                    if existed {
                        self.entry_change -= 1;
                    }
                }
            }
        }
        merged.extend(old);
        Ok(merged)
    }

    fn merge_node(
        &mut self,
        table: TableId,
        page: ChildRef,
        level: u8,
        low: Bytes,
        changes: &[(Bytes, Slot)],
        rightmost: bool,
    ) -> Result<Vec<Child>, EngineError> {
        match self.take_node(table, page, level, &low)? {
            Content::Leaf(entries) => {
                let merged = self.merge_leaf(table, entries, changes)?;
                if merged.is_empty() {
                    return Ok(Vec::new());
                }
                Ok(lows(low, pack(Content::Leaf(merged), rightmost)))
            }
            Content::Branch(entries) => {
                let mut children = Vec::with_capacity(entries.len() + 1);
                let mut rest = changes;
                let count = entries.len();
                for (index, entry) in entries.iter().enumerate() {
                    let split = match entries.get(index + 1) {
                        Some(next) => rest.partition_point(|(key, _)| *key < next.low),
                        None => rest.len(),
                    };
                    let (mine, after) = rest.split_at(split);
                    rest = after;
                    if mine.is_empty() {
                        children.push(Child::Existing {
                            low: entry.low.clone(),
                            page: entry.child,
                        });
                    } else {
                        let last = rightmost && index + 1 == count;
                        children.extend(self.merge_node(
                            table,
                            entry.child,
                            level - 1,
                            entry.low.clone(),
                            mine,
                            last,
                        )?);
                    }
                }
                self.settle(table, level - 1, &mut children)?;
                if children.is_empty() {
                    return Ok(Vec::new());
                }
                let entries = self.write_children(table, level - 1, children)?;
                Ok(lows(low, pack(Content::Branch(entries), rightmost)))
            }
        }
    }

    fn settle(
        &mut self,
        table: TableId,
        level: u8,
        children: &mut Vec<Child>,
    ) -> Result<(), EngineError> {
        let mut index = 0;
        while index < children.len() && children.len() > 1 {
            let small = matches!(&children[index], Child::Fresh { content, .. } if content_bytes(content) < UNDERFULL);
            if !small {
                index += 1;
                continue;
            }
            let left = if index + 1 < children.len() {
                index
            } else {
                index - 1
            };
            let right = children.remove(left + 1);
            let left_child = children.remove(left);
            let low = left_child.low().clone();
            let left_content = self.content_of(table, level, left_child)?;
            let right_content = self.content_of(table, level, right)?;
            let joined = join(vec![left_content, right_content]);
            let pieces = lows(low, pack(joined, false));
            let added = pieces.len();
            for (offset, piece) in pieces.into_iter().enumerate() {
                children.insert(left + offset, piece);
            }
            index = if added > 1 { left + added } else { left };
        }
        Ok(())
    }

    fn content_of(
        &mut self,
        table: TableId,
        level: u8,
        child: Child,
    ) -> Result<Content, EngineError> {
        match child {
            Child::Fresh { content, .. } => Ok(content),
            Child::Existing { low, page } => self.take_node(table, page, level, &low),
        }
    }

    fn write_children(
        &mut self,
        table: TableId,
        level: u8,
        children: Vec<Child>,
    ) -> Result<Vec<BranchEntry>, EngineError> {
        children
            .into_iter()
            .map(|child| match child {
                Child::Existing { low, page } => Ok(BranchEntry { low, child: page }),
                Child::Fresh { low, content } => Ok(BranchEntry {
                    low,
                    child: self.write_node(table, level, &content)?,
                }),
            })
            .collect()
    }

    /// Applies `changes`, every key's newest version in key order, to the tree of `table`, and
    /// returns its new root.
    pub(crate) fn merge_table(
        &mut self,
        table: TableId,
        root: Option<Root>,
        changes: &[(Bytes, Slot)],
    ) -> Result<Option<Root>, EngineError> {
        if changes.is_empty() {
            return Ok(root);
        }
        self.entry_change = 0;
        let empty = Bytes::from(&[][..]);
        let (mut children, mut level) = match root {
            None => {
                let merged = self.merge_leaf(table, Vec::new(), changes)?;
                let children = if merged.is_empty() {
                    Vec::new()
                } else {
                    lows(empty.clone(), pack(Content::Leaf(merged), true))
                };
                (children, 0)
            }
            Some(root) => (
                self.merge_node(table, root.page, root.level, empty.clone(), changes, true)?,
                root.level,
            ),
        };
        let entries = root
            .map_or(0, |root| root.entries)
            .checked_add_signed(self.entry_change)
            .ok_or_else(|| EngineError::corrupt("a table's entry count went below zero"))?;
        loop {
            self.settle(table, level, &mut children)?;
            match children.len() {
                0 => return Ok(None),
                1 => {
                    let child = children.remove(0);
                    let page = match child {
                        Child::Fresh {
                            content: Content::Branch(mut entries),
                            ..
                        } if entries.len() == 1 => {
                            let only = entries.remove(0);
                            children.push(Child::Existing {
                                low: empty.clone(),
                                page: only.child,
                            });
                            level -= 1;
                            continue;
                        }
                        Child::Fresh { content, .. } => self.write_node(table, level, &content)?,
                        Child::Existing { page, .. } => page,
                    };
                    let (page, level) = self.collapse(table, page, level)?;
                    return Ok(Some(Root {
                        page,
                        level,
                        entries,
                    }));
                }
                _ => {
                    if level >= MAX_LEVEL {
                        return Err(EngineError::TooLarge);
                    }
                    let mut entries =
                        self.write_children(table, level, std::mem::take(&mut children))?;
                    if let Some(first) = entries.first_mut() {
                        first.low = empty.clone();
                    }
                    children = lows(empty.clone(), pack(Content::Branch(entries), true));
                    level += 1;
                }
            }
        }
    }

    fn collapse(
        &mut self,
        table: TableId,
        mut page: ChildRef,
        mut level: u8,
    ) -> Result<(ChildRef, u8), EngineError> {
        while level > 0 {
            let cached = fetch(self.source(), table, page, level)?;
            let node = Node::parse(&cached.header, &cached.bytes, table)?;
            if node.cells() > 1 {
                break;
            }
            let child = node.child(0)?;
            self.extents.free(page.at, 1)?;
            page = child;
            level -= 1;
        }
        Ok((page, level))
    }

    /// Writes the catalog for `roots` and this checkpoint's extent table, after freeing
    /// `previous` and dropping every sealed extent left with no live block.
    pub(crate) fn finish(
        mut self,
        roots: BTreeMap<TableId, Root>,
        previous: &[RunRef],
    ) -> Result<Finished, EngineError> {
        for run in previous {
            self.extents.free(run.at, run.blocks)?;
        }
        let dropped = self.extents.drop_empty();
        let mut length = Catalog {
            roots: roots.clone(),
            extents: self.extents.clone(),
        }
        .encode()?
        .len();
        loop {
            let mut planned = self.extents.clone();
            let runs = Self::plan_runs(&mut planned, self.pages, Purpose::Meta, length, self.lsn)?;
            let payload = Catalog {
                roots: roots.clone(),
                extents: planned.clone(),
            }
            .encode()?;
            if payload.len() == length {
                self.extents = planned;
                self.write_runs(&runs, Role::Catalog, TableId(0), &[], &payload)?;
                self.extents.reusable.clear();
                return Ok(Finished {
                    catalog: runs,
                    extents: self.extents,
                    dropped,
                });
            }
            length = payload.len();
        }
    }

    /// Counts every block of the file dead and seals every extent, before [`Self::rebuild`]
    /// copies each table into new extents.
    pub(crate) fn evacuate(&mut self) {
        self.extents.evacuate();
    }

    /// Writes every entry of `table` as `tree` holds it into new, full pages, copying each long
    /// value into a run of its own.
    pub(crate) fn rebuild(
        &mut self,
        table: TableId,
        tree: &Tree,
    ) -> Result<Option<Root>, EngineError> {
        if !tree.roots.contains_key(&table) {
            return Ok(None);
        }
        let mut builder = Builder::default();
        tree.scan::<StorageError>(
            table,
            Bound::Unbounded,
            Bound::Unbounded,
            &mut |key, value| {
                let key = Bytes::from(key);
                let value = self.stored(table, &key, &Bytes::from(value))?;
                builder.push_leaf(self, table, LeafEntry { key, value })?;
                Ok(())
            },
        )?;
        builder.finish(self, table)
    }
}

/// What a checkpoint wrote: the catalog's runs, the extent table it records, and the slots of
/// the extents it dropped.
#[derive(Debug)]
pub(crate) struct Finished {
    pub(crate) catalog: Vec<RunRef>,
    pub(crate) extents: ExtentTable,
    pub(crate) dropped: Vec<u64>,
}

#[derive(Debug, Default)]
struct Level {
    leaves: Vec<LeafEntry>,
    branches: Vec<BranchEntry>,
    size: PageSize,
    low: Bytes,
}

#[derive(Debug, Default)]
struct Builder {
    levels: Vec<Level>,
    entries: u64,
}

impl Builder {
    fn level(&mut self, level: usize) -> &mut Level {
        while self.levels.len() <= level {
            self.levels.push(Level::default());
        }
        &mut self.levels[level]
    }

    fn push_leaf<B: BlockFile>(
        &mut self,
        checkpointer: &mut Checkpointer<'_, B>,
        table: TableId,
        entry: LeafEntry,
    ) -> Result<(), EngineError> {
        self.entries += 1;
        let leaf = self.level(0);
        let (next, bytes) = leaf.size.with(Some(&entry.key), value_bytes(&entry.value));
        if leaf.leaves.is_empty() || bytes <= BLOCK {
            leaf.size = next;
            leaf.leaves.push(entry);
            return Ok(());
        }
        let low = leaf.leaves.last().map_or_else(
            || entry.key.clone(),
            |last| separator(&last.key, &entry.key),
        );
        self.flush(checkpointer, table, 0, low)?;
        let leaf = self.level(0);
        leaf.size = PageSize::default()
            .with(Some(&entry.key), value_bytes(&entry.value))
            .0;
        leaf.leaves.push(entry);
        Ok(())
    }

    fn push_branch<B: BlockFile>(
        &mut self,
        checkpointer: &mut Checkpointer<'_, B>,
        table: TableId,
        level: usize,
        entry: BranchEntry,
    ) -> Result<(), EngineError> {
        let branch = self.level(level);
        if branch.branches.is_empty() {
            branch.low = entry.low.clone();
            branch.size = PageSize::default().with(None, CHILD_BYTES).0;
            branch.branches.push(entry);
            return Ok(());
        }
        let (next, bytes) = branch.size.with(Some(&entry.low), CHILD_BYTES);
        if bytes <= BLOCK {
            branch.size = next;
            branch.branches.push(entry);
            return Ok(());
        }
        self.flush(checkpointer, table, level, entry.low.clone())?;
        self.push_branch(checkpointer, table, level, entry)
    }

    fn flush<B: BlockFile>(
        &mut self,
        checkpointer: &mut Checkpointer<'_, B>,
        table: TableId,
        level: usize,
        low: Bytes,
    ) -> Result<(), EngineError> {
        let current = self.level(level);
        let page_low = std::mem::replace(&mut current.low, low);
        let content = if level == 0 {
            Content::Leaf(std::mem::take(&mut current.leaves))
        } else {
            let mut entries = std::mem::take(&mut current.branches);
            if let Some(first) = entries.first_mut() {
                first.low = page_low.clone();
            }
            Content::Branch(entries)
        };
        current.size = PageSize::default();
        let level_byte = u8::try_from(level).map_err(|_| EngineError::TooLarge)?;
        if level_byte >= MAX_LEVEL {
            return Err(EngineError::TooLarge);
        }
        let child = checkpointer.write_node(table, level_byte, &content)?;
        self.push_branch(
            checkpointer,
            table,
            level + 1,
            BranchEntry {
                low: page_low,
                child,
            },
        )
    }

    fn finish<B: BlockFile>(
        mut self,
        checkpointer: &mut Checkpointer<'_, B>,
        table: TableId,
    ) -> Result<Option<Root>, EngineError> {
        let mut level = 0;
        while level < self.levels.len() {
            let top = level + 1 == self.levels.len();
            let current = &self.levels[level];
            if top {
                if level == 0 {
                    if current.leaves.is_empty() {
                        return Ok(None);
                    }
                } else if current.branches.len() == 1 {
                    let child = current.branches[0].child;
                    return Ok(Some(Root {
                        page: child,
                        level: u8::try_from(level - 1).map_err(|_| EngineError::TooLarge)?,
                        entries: self.entries,
                    }));
                }
                let content = if level == 0 {
                    Content::Leaf(std::mem::take(&mut self.levels[0].leaves))
                } else {
                    let mut entries = std::mem::take(&mut self.levels[level].branches);
                    if let Some(first) = entries.first_mut() {
                        first.low = Bytes::from(&[][..]);
                    }
                    Content::Branch(entries)
                };
                let level_byte = u8::try_from(level).map_err(|_| EngineError::TooLarge)?;
                let page = checkpointer.write_node(table, level_byte, &content)?;
                return Ok(Some(Root {
                    page,
                    level: level_byte,
                    entries: self.entries,
                }));
            }
            let empty = if level == 0 {
                current.leaves.is_empty()
            } else {
                current.branches.is_empty()
            };
            if !empty {
                let low = current.low.clone();
                self.flush(checkpointer, table, level, low)?;
            }
            level += 1;
        }
        Ok(None)
    }
}

/// Reads the catalog a header points to, with the runs it lies in. Its first run lies in an open
/// extent the header names, and each run names the slot of the next, so `place` learns where every
/// later run lies before the extent table is read.
pub(crate) fn load_catalog(
    source: &Arc<dyn PageSource>,
    first: RunRef,
    place: &dyn Fn(ExtentId, u64),
) -> Result<(Catalog, Vec<RunRef>), EngineError> {
    let mut payload = Vec::new();
    let mut runs = Vec::new();
    let mut run = first;
    let mut total = None;
    loop {
        let head = source.run_bytes(RunRef { blocks: 1, ..run })?;
        let length = head
            .get(PAGE_HEADER + 8..PAGE_HEADER + 12)
            .and_then(|bytes| <[u8; 4]>::try_from(bytes).ok())
            .map(u32::from_le_bytes)
            .ok_or_else(|| EngineError::corrupt("the catalog's run header is cut short"))?;
        if length as usize > RUN_PAYLOAD_LIMIT {
            return Err(EngineError::corrupt(
                "the catalog's run claims more than an extent",
            ));
        }
        run.blocks = run_blocks(length as usize);
        let bytes = source.run_bytes(run)?;
        let (_, decoded) = decode_run(source.seed(), &bytes, run, Role::Catalog)?;
        let total = *total.get_or_insert(decoded.total);
        let most_runs = usize::try_from(total / RUN_PAYLOAD_LIMIT as u64 + 1).unwrap_or(usize::MAX);
        if decoded.total != total
            || (payload.len() + decoded.payload.len()) as u64 > total
            || runs.len() >= most_runs
        {
            return Err(EngineError::corrupt(
                "the catalog's runs disagree on its length",
            ));
        }
        payload.extend_from_slice(decoded.payload);
        runs.push(run);
        if decoded.next.at.is_null() {
            break;
        }
        place(decoded.next.at.extent(), decoded.next_slot);
        run = decoded.next;
    }
    if Some(payload.len() as u64) != total {
        return Err(EngineError::corrupt(
            "the catalog is shorter than it claims",
        ));
    }
    Ok((Catalog::decode(&payload)?, runs))
}

#[cfg(test)]
#[path = "checkpoint.test.rs"]
mod tests;
