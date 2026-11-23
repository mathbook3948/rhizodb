use std::ops::Bound;

use super::error::EngineError;
use super::layout::TableId;
use super::memtable::Bytes;
use super::view::Visit;
use crate::storage::StorageError;

/// The tables as the database file holds them at one checkpoint.
#[derive(Debug, Default)]
pub(crate) struct Tree;

impl Tree {
    pub(crate) fn get(&self, _table: TableId, _key: &[u8]) -> Result<Option<Bytes>, EngineError> {
        Ok(None)
    }

    /// Calls `visit` with every entry between the bounds, in key order.
    pub(crate) fn scan<E: From<StorageError>>(
        &self,
        _table: TableId,
        _start: Bound<&[u8]>,
        _end: Bound<&[u8]>,
        _visit: Visit<'_, E>,
    ) -> Result<(), E> {
        Ok(())
    }
}
