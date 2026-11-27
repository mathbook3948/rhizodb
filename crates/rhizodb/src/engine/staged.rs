use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::ops::Bound;
use std::sync::Arc;

use super::error::EngineError;
use super::layout::TableId;
use super::memtable::Bytes;
use super::memtable::Slot;
use super::memtable::bounds_are_ordered;
use super::view::Layers;
use super::view::View;
use super::view::Visit;
use super::view::merge_over;
use super::wal_format::Op;
use super::wal_format::OpRef;
use crate::storage::StorageError;

#[derive(Debug)]
pub(crate) enum StagedOp {
    Put {
        table: TableId,
        key: Bytes,
        value: Bytes,
    },
    Delete {
        table: TableId,
        key: Bytes,
    },
    DeleteRange {
        table: TableId,
        start: Bytes,
        end: Bytes,
    },
}

impl StagedOp {
    pub(crate) fn borrowed(&self) -> OpRef<'_> {
        match self {
            StagedOp::Put { table, key, value } => OpRef::Put {
                table: *table,
                key,
                value,
            },
            StagedOp::Delete { table, key } => OpRef::Delete { table: *table, key },
            StagedOp::DeleteRange { table, start, end } => OpRef::DeleteRange {
                table: *table,
                start,
                end,
            },
        }
    }
}

#[derive(Debug)]
pub(crate) struct Staged {
    pub(crate) view: View,
    overlay: BTreeMap<TableId, BTreeMap<Bytes, Slot>>,
    held_below: BTreeMap<TableId, BTreeSet<Bytes>>,
    pub(crate) ops: Vec<StagedOp>,
}

impl Staged {
    pub(crate) fn new(view: View) -> Self {
        Self {
            view,
            overlay: BTreeMap::new(),
            held_below: BTreeMap::new(),
            ops: Vec::new(),
        }
    }

    pub(crate) fn put(&mut self, table: TableId, key: &[u8], value: &[u8]) {
        let key: Bytes = Arc::from(key);
        let value: Bytes = Arc::from(value);
        self.overlay
            .entry(table)
            .or_default()
            .insert(Arc::clone(&key), Some(Arc::clone(&value)));
        self.ops.push(StagedOp::Put { table, key, value });
    }

    pub(crate) fn delete(&mut self, table: TableId, key: &[u8]) {
        let key: Bytes = Arc::from(key);
        self.overlay
            .entry(table)
            .or_default()
            .insert(Arc::clone(&key), None);
        self.ops.push(StagedOp::Delete { table, key });
    }

    pub(crate) fn delete_range(
        &mut self,
        table: TableId,
        start: &[u8],
        end: &[u8],
    ) -> Result<(), EngineError> {
        if start >= end {
            return Ok(());
        }
        let mut doomed = Vec::new();
        let bounds = (Bound::Included(start), Bound::Excluded(end));
        self.scan(table, bounds.0, bounds.1, &mut |key, _| {
            doomed.push(Arc::<[u8]>::from(key));
            Ok::<_, StorageError>(())
        })?;
        let overlay = self.overlay.entry(table).or_default();
        let held_below = self.held_below.entry(table).or_default();
        let mut active = self
            .view
            .layers
            .active
            .range(table, bounds.0, bounds.1, self.view.snapshot)
            .peekable();
        for key in doomed {
            while active
                .next_if(|(active_key, _)| *active_key < key)
                .is_some()
            {}
            let in_active = active
                .next_if(|(active_key, _)| *active_key == key)
                .is_some();
            if !in_active && !overlay.contains_key(&key) {
                held_below.insert(Arc::clone(&key));
            }
            overlay.insert(key, None);
        }
        self.ops.push(StagedOp::DeleteRange {
            table,
            start: Arc::from(start),
            end: Arc::from(end),
        });
        Ok(())
    }

    pub(crate) fn replay(&mut self, op: Op) -> Result<(), EngineError> {
        match op {
            Op::Put { table, key, value } => {
                table.table()?;
                self.put(table, &key, &value);
            }
            Op::Delete { table, key } => {
                table.table()?;
                self.delete(table, &key);
            }
            Op::DeleteRange { table, start, end } => {
                table.table()?;
                self.delete_range(table, &start, &end)?;
            }
        }
        Ok(())
    }

    /// Calls `read` with the value under `key`, this transaction's own writes included.
    pub(crate) fn get_with<T>(
        &self,
        table: TableId,
        key: &[u8],
        read: impl FnOnce(&[u8]) -> T,
    ) -> Result<Option<T>, EngineError> {
        match self.overlay.get(&table).and_then(|keys| keys.get(key)) {
            Some(slot) => Ok(slot.as_deref().map(read)),
            None => self.view.get_with(table, key, read),
        }
    }

    pub(crate) fn scan<E: From<StorageError>>(
        &self,
        table: TableId,
        start: Bound<&[u8]>,
        end: Bound<&[u8]>,
        visit: Visit<'_, E>,
    ) -> Result<(), E> {
        let upper = self
            .overlay
            .get(&table)
            .filter(|_| bounds_are_ordered(start, end))
            .into_iter()
            .flat_map(|keys| keys.range::<[u8], _>((start, end)))
            .map(|(key, slot)| (Arc::clone(key), slot.clone()));
        merge_over(
            upper,
            |lower| self.view.scan(table, start, end, lower),
            visit,
        )
    }

    /// The keys this transaction deletes that no layer below the active memtable of `layers`
    /// holds, so their tombstones hide nothing.
    pub(crate) fn graves(&self, layers: &Layers) -> Vec<(TableId, Bytes)> {
        self.overlay
            .iter()
            .flat_map(|(table, keys)| keys.iter().map(move |(key, slot)| (*table, key, slot)))
            .filter(|(table, key, slot)| {
                slot.is_none()
                    && !self
                        .held_below
                        .get(table)
                        .is_some_and(|keys| keys.contains(*key))
                    && !layers.below_may_hold(*table, key)
            })
            .map(|(table, key, _)| (table, Arc::clone(key)))
            .collect()
    }

    pub(crate) fn into_changes(self) -> impl Iterator<Item = (TableId, Bytes, Slot)> {
        self.overlay
            .into_iter()
            .flat_map(|(table, keys)| keys.into_iter().map(move |(key, slot)| (table, key, slot)))
    }
}
