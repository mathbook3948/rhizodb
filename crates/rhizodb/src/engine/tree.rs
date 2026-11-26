use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::ops::Bound;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;

use super::blob::BlobRef;
use super::blob::Role;
use super::cache::CachedPage;
use super::catalog::Root;
use super::error::EngineError;
use super::layout::TableId;
#[cfg(test)]
use super::memtable::Bytes;
use super::node::CellKey;
use super::node::CellValue;
use super::node::ChildRef;
use super::node::Node;
use super::page::PageKind;
use super::pages::PageSource;
use super::pages::load_blob;
use super::view::Visit;
use crate::storage::StorageError;

/// The deepest tree the engine reads, so a damaged pointer cycle cannot recurse forever.
pub(crate) const MAX_LEVEL: u8 = 40;

/// The tables as the database file holds them at one checkpoint.
#[derive(Debug)]
pub(crate) struct Tree {
    /// The header generation that published this tree.
    pub(crate) generation: u64,
    pub(crate) roots: BTreeMap<TableId, Root>,
    pub(crate) source: Arc<dyn PageSource>,
    pinned: BTreeMap<TableId, OnceLock<Pinned>>,
}

#[derive(Debug)]
struct Pinned {
    root: CachedPage,
    children: Box<[OnceLock<CachedPage>]>,
}

fn lift<E: From<StorageError>>(error: EngineError) -> E {
    E::from(StorageError::from(error))
}

/// The leaf the last scan of one view started in, with the bounds of the keys it may hold, so
/// a scan that starts within them skips the descent.
#[derive(Debug, Default)]
pub(crate) struct Finger(Mutex<Option<Leaf>>);

#[derive(Debug)]
struct Leaf {
    table: TableId,
    page: CachedPage,
    bounds: Bounds,
}

#[derive(Debug, Default)]
struct Bounds {
    low: Vec<u8>,
    high: Vec<u8>,
    capped: bool,
}

impl Bounds {
    fn covers(&self, target: &[u8]) -> bool {
        self.low.as_slice() <= target && (!self.capped || target < self.high.as_slice())
    }

    fn high(&self) -> Option<&[u8]> {
        self.capped.then_some(self.high.as_slice())
    }
}

#[derive(Debug)]
struct Seek {
    bounds: Bounds,
    leaf: Option<CachedPage>,
    exact: bool,
}

fn set_key(out: &mut Vec<u8>, key: CellKey<'_>) -> bool {
    match key {
        CellKey::Inline { prefix, suffix } => {
            out.clear();
            out.extend_from_slice(prefix);
            out.extend_from_slice(suffix);
            true
        }
        CellKey::Long { .. } => false,
    }
}

impl Seek {
    fn reusing(leaf: Option<Leaf>) -> Self {
        let mut bounds = leaf.map(|leaf| leaf.bounds).unwrap_or_default();
        bounds.low.clear();
        bounds.capped = false;
        Self {
            bounds,
            leaf: None,
            exact: true,
        }
    }

    fn narrow(&mut self, node: &Node<'_>, index: usize) -> Result<(), EngineError> {
        if index > 0 && !set_key(&mut self.bounds.low, node.key(index)?) {
            self.exact = false;
        }
        if index + 1 < node.cells() {
            self.bounds.capped = true;
            if !set_key(&mut self.bounds.high, node.key(index + 1)?) {
                self.exact = false;
            }
        }
        Ok(())
    }

    fn into_leaf(self, table: TableId) -> Option<Leaf> {
        let page = self.leaf.filter(|_| self.exact)?;
        Some(Leaf {
            table,
            page,
            bounds: self.bounds,
        })
    }
}

fn ends_before(end: Bound<&[u8]>, key: &[u8]) -> bool {
    match end {
        Bound::Unbounded => false,
        Bound::Included(end) => end < key,
        Bound::Excluded(end) => end <= key,
    }
}

#[derive(Clone, Copy, Debug)]
struct Range<'a> {
    start: Bound<&'a [u8]>,
    end: Bound<&'a [u8]>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Flow {
    Continue,
    Done,
}

/// Reads the page `page` as a node of `table` at `level`, and fails as corruption unless the
/// page is of that kind, level, table, and the LSN its pointer recorded.
pub(crate) fn fetch(
    source: &dyn PageSource,
    table: TableId,
    page: ChildRef,
    level: u8,
) -> Result<CachedPage, EngineError> {
    if level > MAX_LEVEL {
        return Err(EngineError::corrupt(
            "a tree is deeper than any the engine writes",
        ));
    }
    let cached = source.page(page.at)?;
    let kind = if level == 0 {
        PageKind::Leaf
    } else {
        PageKind::Branch
    };
    cached.header.expect(kind, page.lsn)?;
    if cached.header.level != level || cached.header.table != table {
        return Err(EngineError::corrupt(format!(
            "the page at {:?} is at level {} of table {:#x} where level {level} of {:#x} was expected",
            page.at, cached.header.level, cached.header.table.0, table.0
        )));
    }
    Ok(cached)
}

impl Tree {
    pub(crate) fn new(
        generation: u64,
        roots: BTreeMap<TableId, Root>,
        source: Arc<dyn PageSource>,
    ) -> Self {
        let pinned = roots
            .keys()
            .map(|table| (*table, OnceLock::new()))
            .collect();
        Self {
            generation,
            roots,
            source,
            pinned,
        }
    }

    fn pinned(&self, table: TableId, root: &Root) -> Result<Option<&Pinned>, EngineError> {
        let Some(slot) = self.pinned.get(&table) else {
            return Ok(None);
        };
        if slot.get().is_none() {
            let page = fetch(&*self.source, table, root.page, root.level)?;
            let children = if root.level > 0 {
                Node::parse(&page.header, &page.bytes, table)?.cells()
            } else {
                0
            };
            let _ = slot.set(Pinned {
                root: page,
                children: (0..children).map(|_| OnceLock::new()).collect(),
            });
        }
        Ok(slot.get())
    }

    fn pinned_child<'p>(
        &self,
        pinned: &'p Pinned,
        table: TableId,
        index: usize,
        child: ChildRef,
        level: u8,
    ) -> Result<Option<&'p CachedPage>, EngineError> {
        let Some(slot) = pinned.children.get(index) else {
            return Ok(None);
        };
        if slot.get().is_none() {
            let _ = slot.set(fetch(&*self.source, table, child, level)?);
        }
        Ok(slot.get())
    }

    /// The value stored in `blob`, a run of a value cell of this tree.
    pub(crate) fn load_value(&self, blob: &BlobRef) -> Result<Vec<u8>, EngineError> {
        load_blob(&*self.source, blob, Role::Value)
    }

    pub(crate) fn load_key(&self) -> impl Fn(&BlobRef) -> Result<Vec<u8>, EngineError> + '_ {
        |blob| load_blob(&*self.source, blob, Role::Key)
    }

    #[cfg(test)]
    pub(crate) fn get(&self, table: TableId, key: &[u8]) -> Result<Option<Bytes>, EngineError> {
        self.get_with(table, key, |value| Bytes::from(value))
    }

    /// Calls `read` with the value stored under `key`, borrowed from its page when it is inline.
    pub(crate) fn get_with<T>(
        &self,
        table: TableId,
        key: &[u8],
        read: impl FnOnce(&[u8]) -> T,
    ) -> Result<Option<T>, EngineError> {
        let Some(root) = self.roots.get(&table) else {
            return Ok(None);
        };
        let load = self.load_key();
        let mut page = root.page;
        let mut level = root.level;
        let pinned = self.pinned(table, root)?;
        let mut held = pinned.map(|pinned| &pinned.root);
        let mut at_root = true;
        loop {
            let fetched;
            let cached = match held.take() {
                Some(cached) => cached,
                None => {
                    fetched = fetch(&*self.source, table, page, level)?;
                    &fetched
                }
            };
            let node = Node::parse(&cached.header, &cached.bytes, table)?;
            let (index, exact) = node.find(key, &load)?;
            if node.is_leaf() {
                if !exact {
                    return Ok(None);
                }
                return match node.value(index)? {
                    CellValue::Inline(value) => Ok(Some(read(value))),
                    CellValue::External(blob) => {
                        Ok(Some(read(&load_blob(&*self.source, &blob, Role::Value)?)))
                    }
                };
            }
            page = node.child(index)?;
            level -= 1;
            if let Some(pinned) = pinned.filter(|_| at_root) {
                held = self.pinned_child(pinned, table, index, page, level)?;
            }
            at_root = false;
        }
    }

    /// Calls `visit` with every entry between the bounds, in key order.
    pub(crate) fn scan<E: From<StorageError>>(
        &self,
        table: TableId,
        start: Bound<&[u8]>,
        end: Bound<&[u8]>,
        visit: Visit<'_, E>,
    ) -> Result<(), E> {
        let Some(root) = self.roots.get(&table) else {
            return Ok(());
        };
        self.descend(
            table,
            root,
            Range { start, end },
            visit,
            &mut Vec::new(),
            None,
        )
    }

    /// [`Tree::scan`], starting from the leaf `finger` holds when the start lies within it, and
    /// leaving in `finger` the leaf the start lay in.
    pub(crate) fn scan_near<E: From<StorageError>>(
        &self,
        table: TableId,
        start: Bound<&[u8]>,
        end: Bound<&[u8]>,
        visit: Visit<'_, E>,
        finger: &Finger,
    ) -> Result<(), E> {
        let Some(root) = self.roots.get(&table) else {
            return Ok(());
        };
        let mut key = Vec::new();
        let Ok(mut held) = finger.0.try_lock() else {
            return self.descend(table, root, Range { start, end }, visit, &mut key, None);
        };
        let target = match start {
            Bound::Included(target) | Bound::Excluded(target) => Some(target),
            Bound::Unbounded => None,
        };
        let mut resume = None;
        if let Some(leaf) = held
            .as_ref()
            .filter(|leaf| leaf.table == table && target.is_some_and(|t| leaf.bounds.covers(t)))
        {
            let range = Range { start, end };
            if self.scan_page(table, &leaf.page, 0, None, range, visit, &mut key, None)?
                == Flow::Done
            {
                return Ok(());
            }
            match leaf.bounds.high() {
                Some(high) if !ends_before(end, high) => resume = Some(high.to_vec()),
                _ => return Ok(()),
            }
        }
        let start = resume.as_deref().map_or(start, Bound::Included);
        let mut seek = Seek::reusing(held.take());
        let outcome = self.descend(
            table,
            root,
            Range { start, end },
            visit,
            &mut key,
            Some(&mut seek),
        );
        *held = seek.into_leaf(table);
        outcome
    }

    fn descend<E: From<StorageError>>(
        &self,
        table: TableId,
        root: &Root,
        range: Range<'_>,
        visit: Visit<'_, E>,
        key: &mut Vec<u8>,
        seek: Option<&mut Seek>,
    ) -> Result<(), E> {
        match self.pinned(table, root).map_err(lift)? {
            Some(pinned) => self.scan_page(
                table,
                &pinned.root,
                root.level,
                Some(pinned),
                range,
                visit,
                key,
                seek,
            ),
            None => {
                let page = fetch(&*self.source, table, root.page, root.level).map_err(lift)?;
                self.scan_page(table, &page, root.level, None, range, visit, key, seek)
            }
        }
        .map(|_| ())
    }

    fn past_end(&self, key: &CellKey<'_>, end: Bound<&[u8]>) -> Result<bool, EngineError> {
        let load = self.load_key();
        Ok(match end {
            Bound::Unbounded => false,
            Bound::Included(end) => key.compare(end, &load)? == Ordering::Greater,
            Bound::Excluded(end) => key.compare(end, &load)? != Ordering::Less,
        })
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "the recursion carries the scan's whole state"
    )]
    fn scan_page<E: From<StorageError>>(
        &self,
        table: TableId,
        cached: &CachedPage,
        level: u8,
        pinned: Option<&Pinned>,
        range: Range<'_>,
        visit: Visit<'_, E>,
        key: &mut Vec<u8>,
        seek: Option<&mut Seek>,
    ) -> Result<Flow, E> {
        let Range { start, end } = range;
        let load = self.load_key();
        let node = Node::parse(&cached.header, &cached.bytes, table).map_err(lift)?;
        let first = match start {
            Bound::Unbounded => 0,
            Bound::Included(start) | Bound::Excluded(start) => {
                node.search(start, &load).map_err(lift)?
            }
        };
        if node.is_leaf() {
            if let Some(seek) = seek {
                seek.leaf = Some(Arc::clone(cached));
            }
            for index in first..node.cells() {
                let cell = node.key(index).map_err(lift)?;
                if let Bound::Excluded(start) = start
                    && cell.compare(start, &load).map_err(lift)? == Ordering::Equal
                {
                    continue;
                }
                if self.past_end(&cell, end).map_err(lift)? {
                    return Ok(Flow::Done);
                }
                key.clear();
                match cell {
                    CellKey::Inline { prefix, suffix } => {
                        key.extend_from_slice(prefix);
                        key.extend_from_slice(suffix);
                    }
                    CellKey::Long { .. } => key.extend(cell.full(&load).map_err(lift)?),
                }
                visit(key, node.value(index).map_err(lift)?)?;
            }
            return Ok(Flow::Continue);
        }
        let mut seek = seek;
        for index in first..node.cells() {
            if index > first
                && self
                    .past_end(&node.key(index).map_err(lift)?, end)
                    .map_err(lift)?
            {
                return Ok(Flow::Done);
            }
            let child = node.child(index).map_err(lift)?;
            let from = Range {
                start: if index == first {
                    start
                } else {
                    Bound::Unbounded
                },
                end,
            };
            let mut child_seek = seek.take();
            if let Some(seek) = child_seek.as_deref_mut() {
                seek.narrow(&node, index).map_err(lift)?;
            }
            let held = match pinned {
                Some(pinned) => self
                    .pinned_child(pinned, table, index, child, level - 1)
                    .map_err(lift)?,
                None => None,
            };
            let flow = match held {
                Some(page) => {
                    self.scan_page(table, page, level - 1, None, from, visit, key, child_seek)?
                }
                None => {
                    let page = fetch(&*self.source, table, child, level - 1).map_err(lift)?;
                    self.scan_page(table, &page, level - 1, None, from, visit, key, child_seek)?
                }
            };
            if flow == Flow::Done {
                return Ok(Flow::Done);
            }
        }
        Ok(Flow::Continue)
    }
}
