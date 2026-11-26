use std::collections::BTreeMap;
use std::ops::Bound;
use std::sync::Arc;

use super::error::EngineError;
use super::layout::TableId;
use super::memtable::Bytes;
use super::memtable::Slot;
use super::memtable::bounds_are_ordered;
use super::view::View;
use super::view::Visit;
use super::view::Wanted;
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
    pub(crate) ops: Vec<StagedOp>,
}

impl Staged {
    pub(crate) fn new(view: View) -> Self {
        Self {
            view,
            overlay: BTreeMap::new(),
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
        self.scan(table, bounds.0, bounds.1, None, &mut |key, _| {
            doomed.push(Arc::<[u8]>::from(key));
            Ok::<_, StorageError>(())
        })?;
        let overlay = self.overlay.entry(table).or_default();
        for key in doomed {
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
        wanted: Wanted<'_>,
        visit: Visit<'_, E>,
    ) -> Result<(), E> {
        let upper: Vec<(Bytes, Slot)> = match self.overlay.get(&table) {
            Some(keys) if bounds_are_ordered(start, end) => keys
                .range::<[u8], _>((start, end))
                .map(|(key, slot)| (Arc::clone(key), slot.clone()))
                .collect(),
            _ => Vec::new(),
        };
        merge_over(
            &upper,
            |lower| self.view.scan(table, start, end, wanted, lower),
            visit,
        )
    }

    pub(crate) fn into_changes(self) -> impl Iterator<Item = (TableId, Bytes, Slot)> {
        self.overlay
            .into_iter()
            .flat_map(|(table, keys)| keys.into_iter().map(move |(key, slot)| (table, key, slot)))
    }
}
