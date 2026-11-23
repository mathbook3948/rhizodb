use std::collections::BTreeMap;
use std::collections::BTreeSet;

use crate::storage::ReadTransaction;
use crate::storage::StorageError;
use crate::storage::Table;
use crate::storage::WriteTransaction;
use crate::value::PropertyValue;

use super::GraphError;
use super::GraphTx;
use super::LabelReader;
use super::NodeId;
use super::Properties;
use super::columns;
use super::columns::Cell;
use super::columns::ChunkRef;
use super::columns::Column;
use super::columns::ColumnEdit;
use super::columns::ColumnFlush;
use super::columns::ColumnId;
use super::columns::ColumnRead;
use super::columns::ColumnReader;
use super::columns::Part;
use super::ids::PropertyKey;
use super::ids::Token;
use super::meta;

enum Visit {
    Stop,
    Failed(GraphError),
}

impl From<StorageError> for Visit {
    fn from(error: StorageError) -> Self {
        Visit::Failed(error.into())
    }
}

impl From<GraphError> for Visit {
    fn from(error: GraphError) -> Self {
        Visit::Failed(error)
    }
}

type StoredColumn = (Option<Column>, Vec<(usize, Option<PropertyValue>)>);

/// A write to the column table.
pub(crate) enum ColumnWrite {
    /// Stores the value under the key.
    Put([u8; 16], Vec<u8>),
    /// Deletes the key.
    Delete([u8; 16]),
    /// Deletes every delta of the column.
    DeleteDeltas(ColumnId),
}

impl<T: ReadTransaction> GraphTx<T> {
    fn load_column(&self, (group, token): ColumnId) -> Result<StoredColumn, GraphError> {
        let mut chunk = None;
        let mut deltas = Vec::new();
        self.transaction.visit_prefix(
            Table::NodeColumns,
            &columns::column_prefix(group, token),
            |key, value| {
                match columns::parse_key(key)?.2 {
                    Part::Chunk => chunk = Some(Column::load(group, token, value)?),
                    Part::Delta(_) if chunk.is_none() => {
                        return Err(GraphError::Corrupt("a property column delta has no chunk"));
                    }
                    Part::Delta(_) => deltas.push(columns::parse_delta(key, value)?),
                }
                Ok::<(), GraphError>(())
            },
        )?;
        Ok((chunk, deltas))
    }

    fn probe_column(
        &self,
        (group, token): ColumnId,
        slot: usize,
    ) -> Result<Option<PropertyValue>, GraphError> {
        let mut found: Option<Option<PropertyValue>> = None;
        let visited = self.transaction.visit_prefix(
            Table::NodeColumns,
            &columns::column_prefix(group, token),
            |key, value| match columns::parse_key(key)?.2 {
                Part::Chunk => {
                    let chunk = ChunkRef::parse(group, token, value)?;
                    found = Some(chunk.cell_at(slot)?.map(Cell::to_value).transpose()?);
                    Ok(())
                }
                Part::Delta(_) if found.is_none() => Err(Visit::Failed(GraphError::Corrupt(
                    "a property column delta has no chunk",
                ))),
                Part::Delta(at) if at < slot => Ok(()),
                Part::Delta(at) => {
                    if at == slot {
                        found = Some(columns::parse_delta(key, value)?.1);
                    }
                    Err(Visit::Stop)
                }
            },
        );
        match visited {
            Ok(()) | Err(Visit::Stop) => Ok(found.flatten()),
            Err(Visit::Failed(error)) => Err(error),
        }
    }

    fn stored_value(
        &self,
        reader: &mut ColumnReader,
        id: ColumnId,
        slot: usize,
    ) -> Result<Option<PropertyValue>, GraphError> {
        match reader.read(id, slot)? {
            ColumnRead::Value(value) => Ok(value),
            ColumnRead::Load(cached) => {
                let (chunk, deltas) = self.load_column(id)?;
                cached.fill(chunk, deltas, slot)
            }
            ColumnRead::Probe => self.probe_column(id, slot),
        }
    }

    /// The value of the property `token` of the node in `slot` of `group` as the transaction
    /// sees it. A slot without a live node holds no value.
    pub(crate) fn column_value(
        &self,
        reader: &mut ColumnReader,
        group: u64,
        slot: usize,
        token: Token<PropertyKey>,
    ) -> Result<Option<PropertyValue>, GraphError> {
        if let Some(edit) = self.columns.get(&(group, token))
            && let Some(value) = edit.current(slot)
        {
            return Ok(value.cloned());
        }
        self.stored_value(reader, (group, token), slot)
    }

    fn stored_keys(&self, group: u64) -> Result<Vec<Token<PropertyKey>>, GraphError> {
        let mut keys: Vec<Token<PropertyKey>> = Vec::new();
        self.transaction.visit_prefix(
            Table::NodeColumns,
            &columns::group_prefix(group),
            |key, _| {
                let (_, token, _) = columns::parse_key(key)?;
                if keys.last() != Some(&token) {
                    keys.push(token);
                }
                Ok::<(), GraphError>(())
            },
        )?;
        Ok(keys)
    }

    fn group_keys(
        &self,
        reader: &mut ColumnReader,
        group: u64,
    ) -> Result<BTreeSet<Token<PropertyKey>>, GraphError> {
        if reader.keys(group).is_none() {
            let keys = self.stored_keys(group)?;
            reader.insert_keys(group, keys);
        }
        let mut keys: BTreeSet<Token<PropertyKey>> = reader
            .keys(group)
            .unwrap_or_default()
            .iter()
            .copied()
            .collect();
        keys.extend(
            self.columns
                .range((group, Token::new(0))..=(group, Token::new(u32::MAX)))
                .map(|((_, token), _)| *token),
        );
        Ok(keys)
    }

    /// Every property of the node with ID `id`, which must be live.
    pub(crate) fn live_node_properties(
        &self,
        reader: &mut ColumnReader,
        id: NodeId,
    ) -> Result<Properties, GraphError> {
        let (group, slot) = meta::locate(id);
        let mut properties = Properties::new();
        for token in self.group_keys(reader, group)? {
            if let Some(value) = self.column_value(reader, group, slot, token)? {
                properties.insert(token, value);
            }
        }
        Ok(properties)
    }

    /// One property of the node with ID `id`. The outer `None` means there is no such node. A
    /// `key` of `None` names a property no node holds.
    pub(crate) fn node_property(
        &self,
        labels: &mut LabelReader,
        columns: &mut ColumnReader,
        id: NodeId,
        key: Option<Token<PropertyKey>>,
    ) -> Result<Option<Option<PropertyValue>>, GraphError> {
        if let Some(key) = key {
            let (group, slot) = meta::locate(id);
            if let Some(value) = self.column_value(columns, group, slot, key)? {
                return Ok(Some(Some(value)));
            }
        }
        Ok(self.with_labels(labels, id, |_| ())?.map(|()| None))
    }

    /// Every property of the node with ID `id`, or `None` when there is no such node.
    pub(crate) fn node_properties(
        &self,
        labels: &mut LabelReader,
        columns: &mut ColumnReader,
        id: NodeId,
    ) -> Result<Option<Properties>, GraphError> {
        if self.with_labels(labels, id, |_| ())?.is_none() {
            return Ok(None);
        }
        self.live_node_properties(columns, id).map(Some)
    }

    /// What commit writes for one edited column: a rewritten chunk, or the deltas that differ
    /// from the stored chunk.
    pub(crate) fn column_writes(
        &self,
        (group, token): ColumnId,
        edit: ColumnEdit,
    ) -> Result<Vec<ColumnWrite>, GraphError> {
        let key = columns::chunk_key(group, token);
        let mut writes = Vec::new();
        match edit.flush() {
            ColumnFlush::Rewrite { values, stale } => {
                let mut changes = BTreeMap::new();
                if !stale.is_empty() {
                    self.transaction.visit_prefix(
                        Table::NodeColumns,
                        &columns::column_prefix(group, token),
                        |key, value| {
                            if let Part::Delta(slot) = columns::parse_key(key)?.2
                                && !values.contains_key(&slot)
                            {
                                let (slot, value) = columns::parse_delta(key, value)?;
                                changes.insert(slot, value);
                            }
                            Ok::<(), GraphError>(())
                        },
                    )?;
                }
                changes.extend(values);
                let rewritten = self
                    .transaction
                    .get_with(Table::NodeColumns, &key, |bytes| {
                        let chunk = ChunkRef::parse(group, token, bytes)?;
                        chunk.verify()?;
                        columns::encode_chunk(
                            group,
                            token,
                            &columns::merged(chunk.cells(), &changes)?,
                        )
                    })?;
                let chunk = match rewritten {
                    Some(chunk) => chunk,
                    None if stale.is_empty() => columns::encode_chunk(
                        group,
                        token,
                        &columns::merged(std::iter::empty(), &changes)?,
                    )?,
                    None => return Err(GraphError::Corrupt("a property column lost its chunk")),
                };
                writes.push(match chunk {
                    Some(chunk) => ColumnWrite::Put(key, chunk),
                    None => ColumnWrite::Delete(key),
                });
                if !stale.is_empty() {
                    writes.push(ColumnWrite::DeleteDeltas((group, token)));
                }
            }
            ColumnFlush::Deltas { touched, stored } => {
                let mut compare = |chunk: Option<ChunkRef<'_>>| {
                    for (slot, had, after) in &touched {
                        let before = match chunk {
                            Some(chunk) if *had => chunk.cell_at(*slot)?,
                            _ => None,
                        };
                        let after = after.as_ref().map(Cell::of);
                        if !columns::same_state(before, after) {
                            writes.push(ColumnWrite::Put(
                                columns::delta_key(group, token, *slot),
                                columns::encode_delta(group, token, *slot, after),
                            ));
                        } else if stored.binary_search(slot).is_ok() {
                            writes
                                .push(ColumnWrite::Delete(columns::delta_key(group, token, *slot)));
                        }
                    }
                    Ok::<(), GraphError>(())
                };
                if touched.iter().any(|(_, had, _)| *had) {
                    self.transaction
                        .get_with(Table::NodeColumns, &key, |bytes| {
                            compare(Some(ChunkRef::parse(group, token, bytes)?))
                        })?
                        .ok_or(GraphError::Corrupt("a property column lost its chunk"))?;
                } else {
                    compare(None)?;
                }
            }
        }
        Ok(writes)
    }
}

impl<T: WriteTransaction> GraphTx<T> {
    fn load_edit(&self, (group, token): ColumnId) -> Result<ColumnEdit, GraphError> {
        let mut head = None;
        let mut deltas = Vec::new();
        self.transaction.visit_prefix(
            Table::NodeColumns,
            &columns::column_prefix(group, token),
            |key, value| {
                match columns::parse_key(key)?.2 {
                    Part::Chunk => head = Some(ChunkRef::parse(group, token, value)?.head()),
                    Part::Delta(_) if head.is_none() => {
                        return Err(GraphError::Corrupt("a property column delta has no chunk"));
                    }
                    Part::Delta(_) => deltas.push(columns::delta_state(key, value)?),
                }
                Ok::<(), GraphError>(())
            },
        )?;
        ColumnEdit::load(head, deltas)
    }

    /// Sets the value of the property `token` of the node in `slot` of `group`, or removes it
    /// with `None`.
    pub(crate) fn set_column_value(
        &mut self,
        (group, token): ColumnId,
        slot: usize,
        value: Option<PropertyValue>,
    ) -> Result<(), GraphError> {
        if !self.columns.contains_key(&(group, token)) {
            let edit = self.load_edit((group, token))?;
            self.columns.insert((group, token), edit);
        }
        self.columns
            .get_mut(&(group, token))
            .ok_or(GraphError::Corrupt("a property column edit vanished"))?
            .set(slot, value);
        Ok(())
    }

    /// Reads through the write transaction's own reader, which keeps the stored columns it
    /// read for the rest of the transaction.
    pub(crate) fn with_reader<R>(
        &mut self,
        read: impl FnOnce(&Self, &mut ColumnReader) -> Result<R, GraphError>,
    ) -> Result<R, GraphError> {
        let mut reader = std::mem::take(&mut self.reader);
        let result = read(self, &mut reader);
        self.reader = reader;
        result
    }

    pub(crate) fn flush_columns(&mut self) -> Result<(), GraphError> {
        for (id, edit) in std::mem::take(&mut self.columns) {
            for write in self.column_writes(id, edit)? {
                match write {
                    ColumnWrite::Put(key, value) => {
                        self.transaction.put(Table::NodeColumns, &key, &value)?;
                    }
                    ColumnWrite::Delete(key) => {
                        self.transaction.delete(Table::NodeColumns, &key)?
                    }
                    ColumnWrite::DeleteDeltas((group, token)) => {
                        let (start, end) = columns::delta_range(group, token);
                        self.transaction
                            .delete_range(Table::NodeColumns, &start, &end)?;
                    }
                }
            }
        }
        Ok(())
    }
}
