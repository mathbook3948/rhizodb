use std::cmp::Ordering;
use std::ops::Bound;
use std::sync::Arc;

use super::blob::BlobRef;
use super::cache::Access;
use super::error::EngineError;
use super::layout::TableId;
use super::memtable::Bytes;
use super::memtable::Lookup;
use super::memtable::Memtable;
use super::memtable::Slot;
use super::node::CellValue;
use super::tree::Finger;
use super::tree::Tree;
use crate::storage::StorageError;

/// The layers a snapshot reads, newest first: the memtable commits go to, the memtable a
/// checkpoint is writing out, and the tree of the last checkpoint.
#[derive(Debug)]
pub(crate) struct Layers {
    pub(crate) active: Arc<Memtable>,
    pub(crate) frozen: Option<Arc<Memtable>>,
    pub(crate) tree: Arc<Tree>,
}

impl Layers {
    /// Whether a layer below the active memtable may hold a value under `key`, which a
    /// tombstone in the active memtable must then keep hidden.
    pub(crate) fn below_may_hold(&self, table: TableId, key: &[u8]) -> bool {
        if let Some(frozen) = &self.frozen {
            match frozen.get(table, key, u64::MAX) {
                Lookup::Found(_) => return true,
                Lookup::Deleted => return false,
                Lookup::Absent => {}
            }
        }
        !matches!(self.tree.get_with(table, key, |_| ()), Ok(None))
    }
}

/// The database as committed at one LSN.
#[derive(Debug)]
pub(crate) struct View {
    pub(crate) snapshot: u64,
    pub(crate) layers: Arc<Layers>,
    finger: Finger,
}

impl Clone for View {
    fn clone(&self) -> Self {
        View::new(self.snapshot, Arc::clone(&self.layers))
    }
}

/// What a scan calls with each entry: its key, and its value in place or, for a value that lives
/// in a run of its own, the run, which the scan does not read.
pub(crate) type Visit<'a, E> = &'a mut dyn FnMut(&[u8], CellValue<'_>) -> Result<(), E>;

impl View {
    pub(crate) fn new(snapshot: u64, layers: Arc<Layers>) -> Self {
        Self {
            snapshot,
            layers,
            finger: Finger::default(),
        }
    }

    /// Calls `read` with the value the snapshot holds under `key`.
    pub(crate) fn get_with<T>(
        &self,
        table: TableId,
        key: &[u8],
        read: impl FnOnce(&[u8]) -> T,
    ) -> Result<Option<T>, EngineError> {
        let memtables = std::iter::once(&self.layers.active).chain(&self.layers.frozen);
        for memtable in memtables {
            match memtable.get(table, key, self.snapshot) {
                Lookup::Found(value) => return Ok(Some(read(&value))),
                Lookup::Deleted => return Ok(None),
                Lookup::Absent => {}
            }
        }
        self.layers.tree.get_with(table, key, read)
    }

    /// The value stored in `blob`, a run a scan of the snapshot met.
    pub(crate) fn load(&self, blob: &BlobRef, access: Access) -> Result<Arc<[u8]>, EngineError> {
        self.layers.tree.load_value(blob, access)
    }

    /// Calls `visit` with every entry between the bounds, in key order.
    pub(crate) fn scan<E: From<StorageError>>(
        &self,
        table: TableId,
        start: Bound<&[u8]>,
        end: Bound<&[u8]>,
        visit: Visit<'_, E>,
    ) -> Result<(), E> {
        let active = self.layers.active.range(table, start, end, self.snapshot);
        let frozen = self
            .layers
            .frozen
            .iter()
            .flat_map(|frozen| frozen.range(table, start, end, self.snapshot));
        merge_over(
            merge_slots(active, frozen),
            |lower| {
                self.layers
                    .tree
                    .scan_near(table, start, end, lower, &self.finger)
            },
            visit,
        )
    }
}

/// `newer` and `older` merged in key order, `newer` winning on a key both hold.
pub(crate) fn merge_slots(
    newer: impl Iterator<Item = (Bytes, Slot)>,
    older: impl Iterator<Item = (Bytes, Slot)>,
) -> impl Iterator<Item = (Bytes, Slot)> {
    let mut newer = newer.peekable();
    let mut older = older.peekable();
    std::iter::from_fn(move || {
        let Some((newer_key, _)) = newer.peek() else {
            return older.next();
        };
        match older.peek().map(|(older_key, _)| older_key.cmp(newer_key)) {
            Some(Ordering::Less) => older.next(),
            Some(Ordering::Equal) => {
                older.next();
                newer.next()
            }
            Some(Ordering::Greater) | None => newer.next(),
        }
    })
}

/// Visits the entries `lower` yields with `upper` laid over them: an upper value replaces the
/// lower one of its key, and an upper tombstone hides it. It takes from `upper` only as far as
/// the visit gets.
pub(crate) fn merge_over<E>(
    upper: impl Iterator<Item = (Bytes, Slot)>,
    lower: impl FnOnce(Visit<'_, E>) -> Result<(), E>,
    visit: Visit<'_, E>,
) -> Result<(), E> {
    let mut upper = upper.peekable();
    lower(&mut |key, value| {
        while let Some((upper_key, _)) = upper.peek() {
            let order = (**upper_key).cmp(key);
            if order == Ordering::Greater {
                break;
            }
            if let Some((upper_key, Some(upper_value))) = upper.next() {
                visit(&upper_key, CellValue::Inline(&upper_value))?;
            }
            if order == Ordering::Equal {
                return Ok(());
            }
        }
        visit(key, value)
    })?;
    for (key, slot) in upper {
        if let Some(value) = slot {
            visit(&key, CellValue::Inline(&value))?;
        }
    }
    Ok(())
}

/// The smallest key above every key that starts with `prefix`, or `None` when there is none.
pub(crate) fn prefix_end(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut end = prefix.to_vec();
    while let Some(last) = end.pop() {
        if last < u8::MAX {
            end.push(last + 1);
            return Some(end);
        }
    }
    None
}

#[cfg(test)]
#[path = "view.test.rs"]
mod tests;
