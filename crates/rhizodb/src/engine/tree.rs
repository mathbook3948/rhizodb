use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::ops::Bound;
use std::sync::Arc;
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
    root_pages: BTreeMap<TableId, OnceLock<CachedPage>>,
}

fn lift<E: From<StorageError>>(error: EngineError) -> E {
    E::from(StorageError::from(error))
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
        let root_pages = roots
            .keys()
            .map(|table| (*table, OnceLock::new()))
            .collect();
        Self {
            generation,
            roots,
            source,
            root_pages,
        }
    }

    fn root_page(&self, table: TableId, root: &Root) -> Result<Option<&CachedPage>, EngineError> {
        let Some(pinned) = self.root_pages.get(&table) else {
            return Ok(None);
        };
        if pinned.get().is_none() {
            let page = fetch(&*self.source, table, root.page, root.level)?;
            let _ = pinned.set(page);
        }
        Ok(pinned.get())
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
        let mut pinned = self.root_page(table, root)?;
        loop {
            let fetched;
            let cached = match pinned.take() {
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
        let mut key = Vec::new();
        self.scan_node(table, root.page, root.level, start, end, visit, &mut key)
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
    fn scan_node<E: From<StorageError>>(
        &self,
        table: TableId,
        page: ChildRef,
        level: u8,
        start: Bound<&[u8]>,
        end: Bound<&[u8]>,
        visit: Visit<'_, E>,
        key: &mut Vec<u8>,
    ) -> Result<Flow, E> {
        let load = self.load_key();
        let cached = fetch(&*self.source, table, page, level).map_err(lift)?;
        let node = Node::parse(&cached.header, &cached.bytes, table).map_err(lift)?;
        let first = match start {
            Bound::Unbounded => 0,
            Bound::Included(start) | Bound::Excluded(start) => {
                node.search(start, &load).map_err(lift)?
            }
        };
        if node.is_leaf() {
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
                match node.value(index).map_err(lift)? {
                    CellValue::Inline(value) => visit(key, value)?,
                    CellValue::External(blob) => {
                        let value = load_blob(&*self.source, &blob, Role::Value).map_err(lift)?;
                        visit(key, &value)?;
                    }
                }
            }
            return Ok(Flow::Continue);
        }
        for index in first..node.cells() {
            if index > first
                && self
                    .past_end(&node.key(index).map_err(lift)?, end)
                    .map_err(lift)?
            {
                return Ok(Flow::Done);
            }
            let child = node.child(index).map_err(lift)?;
            let from = if index == first {
                start
            } else {
                Bound::Unbounded
            };
            if self.scan_node(table, child, level - 1, from, end, visit, key)? == Flow::Done {
                return Ok(Flow::Done);
            }
        }
        Ok(Flow::Continue)
    }
}
