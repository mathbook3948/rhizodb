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
use super::columns::Cached;
use super::columns::Cell;
use super::columns::Changes;
use super::columns::Column;
use super::columns::ColumnId;
use super::columns::ColumnKey;
use super::columns::ColumnReader;
use super::columns::ColumnsEdit;
use super::columns::Delta;
use super::columns::DeltaRef;
use super::columns::Deltas;
use super::columns::IndexedColumn;
use super::columns::Probes;
use super::columns::Quick;
use super::columns::SEGMENT_SLOTS;
use super::columns::Segment;
use super::columns::SegmentRef;
use super::ids::PropertyKey;
use super::ids::Token;
use super::lock;
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

const DELTA_UNCOUNTED: GraphError =
    GraphError::Corrupt("a property column delta is missing from the count of its group");

/// A write to the column table.
#[derive(Debug)]
pub(crate) enum ColumnWrite {
    /// Stores the value under the key.
    Put(Vec<u8>, Vec<u8>),
    /// Deletes the key.
    Delete(Vec<u8>),
    /// Deletes every delta of the group.
    DeleteDeltas(u64),
}

impl ColumnWrite {
    fn key(&self) -> Option<&[u8]> {
        match self {
            ColumnWrite::Put(key, _) | ColumnWrite::Delete(key) => Some(key),
            ColumnWrite::DeleteDeltas(_) => None,
        }
    }
}

fn delta_changes(key: &[u8], bytes: &[u8]) -> Result<Changes, GraphError> {
    DeltaRef::parse(key, bytes)?
        .entries()
        .map(|entry| {
            let (token, cell) = entry?;
            Ok((token, cell.map(Cell::to_value).transpose()?))
        })
        .collect()
}

fn state(
    delta: &Delta,
    token: Token<PropertyKey>,
) -> Result<Option<Option<PropertyValue>>, GraphError> {
    delta
        .get(token)?
        .map(|cell| cell.map(Cell::to_value).transpose())
        .transpose()
}

/// Some columns of one row group read whole, which give the value of each as a transaction sees
/// it, in place.
#[derive(Debug)]
pub(crate) struct ScanGroup<'t> {
    group: u64,
    edit: Option<&'t ColumnsEdit>,
    deltas: Deltas,
    columns: Vec<Option<(Token<PropertyKey>, IndexedColumn)>>,
}

impl ScanGroup<'_> {
    /// Whether `node` lies in the row group.
    pub(crate) fn holds(&self, node: NodeId) -> bool {
        meta::locate(node).0 == self.group
    }

    /// The value of column `column` of the live node `node` where it is absent or a stored
    /// integer, read without decoding.
    pub(crate) fn quick(&self, node: NodeId, column: usize) -> Quick {
        if self.edit.is_some() || !self.deltas.is_empty() {
            return Quick::Unknown;
        }
        match self.columns.get(column) {
            Some(Some((_, stored))) => stored.quick(meta::locate(node).1),
            _ => Quick::Absent,
        }
    }

    /// The value of column `column` of the live node `node`, which the group must hold.
    pub(crate) fn cell(&self, node: NodeId, column: usize) -> Result<Option<Cell<'_>>, GraphError> {
        let Some(Some((token, stored))) = self.columns.get(column) else {
            return Ok(None);
        };
        let (_, slot) = meta::locate(node);
        if let Some(edit) = self.edit
            && let Some(cell) = edit.current_cell(slot, *token)?
        {
            return Ok(cell);
        }
        if let Some(delta) = columns::find_delta(&self.deltas, slot)
            && let Some(cell) = delta.view().get(*token)?
        {
            return Ok(cell);
        }
        stored.cell_at(slot)
    }
}

impl<T: ReadTransaction> GraphTx<T> {
    fn load_column(&self, (group, token): ColumnId) -> Result<Column, GraphError> {
        let mut column = Column::new();
        self.transaction.visit_prefix(
            Table::NodeColumns,
            &columns::column_prefix(group, token),
            |key, value| {
                match columns::parse_key(key)? {
                    ColumnKey::Segment { segment, .. } => {
                        column.insert(segment, Segment::load(key, value)?);
                    }
                    ColumnKey::Delta { .. } => {
                        return Err(GraphError::Corrupt(
                            "a property column delta lies among segments",
                        ));
                    }
                }
                Ok::<(), GraphError>(())
            },
        )?;
        Ok(column)
    }

    fn load_deltas(&self, group: u64) -> Result<Deltas, GraphError> {
        let mut deltas = Deltas::new();
        self.transaction.visit_prefix(
            Table::NodeColumns,
            &columns::deltas_prefix(group),
            |key, value| {
                let ColumnKey::Delta { slot, .. } = columns::parse_key(key)? else {
                    return Err(GraphError::Corrupt(
                        "a property column segment lies among deltas",
                    ));
                };
                deltas.push((slot, Delta::load(key, value)?));
                Ok::<(), GraphError>(())
            },
        )?;
        Ok(deltas)
    }

    fn probe_delta(&self, group: u64, slot: usize) -> Result<Option<Delta>, GraphError> {
        let key = columns::delta_key(group, slot);
        self.transaction
            .get_with(Table::NodeColumns, &key, |bytes| Delta::load(&key, bytes))
    }

    fn stored_delta<'r>(
        &self,
        reader: &'r mut ColumnReader,
        group: u64,
        slot: usize,
    ) -> Result<Option<&'r Delta>, GraphError> {
        let read = match reader.deltas.get(&group) {
            Some(Cached::Probed((probed, _))) if *probed != slot => {
                Some(Cached::Loaded(self.load_deltas(group)?))
            }
            Some(_) => None,
            None => Some(Cached::Probed((slot, self.probe_delta(group, slot)?))),
        };
        let cached = match read {
            Some(read) => Some(reader.deltas.insert(group, read)),
            None => reader.deltas.get(&group),
        };
        Ok(match cached {
            Some(Cached::Probed((_, delta))) => delta.as_ref(),
            Some(Cached::Loaded(deltas)) => columns::find_delta(deltas, slot),
            None => None,
        })
    }

    fn segment_cell<R>(
        &self,
        (group, token): ColumnId,
        slot: usize,
        read: impl Fn(Option<Cell<'_>>) -> Result<R, GraphError>,
    ) -> Result<R, GraphError> {
        let segment = slot / SEGMENT_SLOTS;
        let at = slot % SEGMENT_SLOTS;
        let key = columns::segment_key(group, token, segment, 0);
        let first = self
            .transaction
            .get_with(Table::NodeColumns, &key, |bytes| {
                let part = SegmentRef::parse(&key, bytes)?;
                Ok::<_, GraphError>(match part.part_of(at) {
                    0 => Ok(read(part.cell_at(at)?)?),
                    other => Err(other),
                })
            })?;
        let part = match first {
            None => return read(None),
            Some(Ok(done)) => return Ok(done),
            Some(Err(part)) => part,
        };
        let key = columns::segment_key(group, token, segment, part);
        match self
            .transaction
            .get_with(Table::NodeColumns, &key, |bytes| {
                read(SegmentRef::parse(&key, bytes)?.cell_at(at)?)
            })? {
            Some(done) => Ok(done),
            None => read(None),
        }
    }

    fn segment_value(
        &self,
        reader: &mut ColumnReader,
        id: ColumnId,
        slot: usize,
    ) -> Result<Option<PropertyValue>, GraphError> {
        self.segment_read(reader, id, slot, |cell| {
            cell.map(Cell::to_value).transpose()
        })
    }

    fn segment_read<R>(
        &self,
        reader: &mut ColumnReader,
        id: ColumnId,
        slot: usize,
        read: impl Fn(Option<Cell<'_>>) -> Result<R, GraphError>,
    ) -> Result<R, GraphError> {
        if let Some(column) = reader.columns.get(&id) {
            return read(column.cell_at(slot)?);
        }
        let probes = match reader.probes.get(&id) {
            Some(probes) => probes,
            None => reader.probes.insert(id, Probes::Read(0)),
        };
        match probes {
            Probes::Read(reads) if *reads < columns::PROBES => {
                *reads += 1;
                return self.segment_cell(id, slot, read);
            }
            Probes::Read(_) => {}
            Probes::Loaded => return self.segment_cell(id, slot, read),
        }
        let column = self.load_column(id)?;
        *probes = Probes::Loaded;
        let value = column.cell_at(slot).and_then(read);
        reader.columns.insert(id, column);
        value
    }

    fn stored_value(
        &self,
        reader: &mut ColumnReader,
        (group, token): ColumnId,
        slot: usize,
    ) -> Result<Option<PropertyValue>, GraphError> {
        if let Some(delta) = self.stored_delta(reader, group, slot)?
            && let Some(value) = state(delta, token)?
        {
            return Ok(value);
        }
        self.segment_value(reader, (group, token), slot)
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
        if let Some(edit) = self.columns.get(&group)
            && let Some(value) = edit.current(slot, token)?
        {
            return Ok(value);
        }
        self.stored_value(reader, (group, token), slot)
    }

    /// Reads the columns `tokens` of the row group that holds `node` whole, for a scan of its
    /// nodes.
    pub(crate) fn scan_group(
        &self,
        node: NodeId,
        tokens: &[Option<Token<PropertyKey>>],
    ) -> Result<ScanGroup<'_>, GraphError> {
        let (group, _) = meta::locate(node);
        let columns = tokens
            .iter()
            .map(|token| {
                token
                    .map(|token| Ok((token, IndexedColumn::new(self.load_column((group, token))?))))
                    .transpose()
            })
            .collect::<Result<_, GraphError>>()?;
        Ok(ScanGroup {
            group,
            edit: self.columns.get(&group),
            deltas: self.load_deltas(group)?,
            columns,
        })
    }

    fn stored_keys(&self, group: u64) -> Result<Vec<Token<PropertyKey>>, GraphError> {
        let mut keys: Vec<Token<PropertyKey>> = Vec::new();
        let end = columns::segments_end(group);
        let mut next = Some(Token::<PropertyKey>::new(0));
        while let Some(from) = next.take() {
            let visited = self.transaction.visit_range(
                Table::NodeColumns,
                &columns::segment_key(group, from, 0, 0),
                &end,
                |key, _| match columns::parse_key(key)? {
                    ColumnKey::Segment { token, .. } => {
                        keys.push(token);
                        next = token.id().checked_add(1).map(Token::new);
                        Err(Visit::Stop)
                    }
                    ColumnKey::Delta { .. } => Err(Visit::Failed(GraphError::Corrupt(
                        "a property column delta lies among segments",
                    ))),
                },
            );
            if let Err(Visit::Failed(error)) = visited {
                return Err(error);
            }
        }
        Ok(keys)
    }

    fn node_keys(
        &self,
        reader: &mut ColumnReader,
        group: u64,
        slot: usize,
    ) -> Result<BTreeSet<Token<PropertyKey>>, GraphError> {
        let stored = match reader.keys.get(&group) {
            Some(stored) => stored,
            None => {
                let stored = self.stored_keys(group)?;
                reader.keys.insert(group, stored)
            }
        };
        let mut keys: BTreeSet<Token<PropertyKey>> = stored.iter().copied().collect();
        if let Some(delta) = self.stored_delta(reader, group, slot)? {
            for token in delta.view().tokens() {
                keys.insert(token?);
            }
        }
        if let Some(edit) = self.columns.get(&group) {
            keys.extend(edit.tokens(slot)?);
        }
        Ok(keys)
    }

    /// Every property of the node with ID `id`, which must be live.
    pub(crate) fn live_node_properties(
        &self,
        reader: &mut ColumnReader,
        id: NodeId,
    ) -> Result<Properties, GraphError> {
        let (group, slot) = meta::locate(id);
        let (mut edited, fresh) = match self.columns.get(&group) {
            Some(edit) => edit.current_all(slot)?,
            None => (BTreeMap::new(), false),
        };
        let mut stored = BTreeMap::new();
        if !fresh && let Some(delta) = self.stored_delta(reader, group, slot)? {
            for entry in delta.view().entries() {
                let (token, cell) = entry?;
                stored.insert(token, cell.map(Cell::to_value).transpose()?);
            }
        }
        let mut properties = Properties::new();
        for token in self.node_keys(reader, group, slot)? {
            let value = if let Some(value) = edited.remove(&token) {
                value
            } else if fresh {
                None
            } else if let Some(value) = stored.remove(&token) {
                value
            } else {
                self.segment_value(reader, (group, token), slot)?
            };
            if let Some(value) = value {
                properties.insert(token, value);
            }
        }
        Ok(properties)
    }

    /// The keys of every property of the node with ID `id`, ascending, decoding none of the
    /// values. `None` means there is no such node.
    pub(crate) fn node_property_keys(
        &self,
        labels: &mut LabelReader,
        reader: &mut ColumnReader,
        id: NodeId,
    ) -> Result<Option<Vec<Token<PropertyKey>>>, GraphError> {
        if self.with_labels(labels, id, |_| ())?.is_none() {
            return Ok(None);
        }
        let (group, slot) = meta::locate(id);
        let edit = self.columns.get(&group);
        let mut keys = Vec::new();
        for token in self.node_keys(reader, group, slot)? {
            let held = if let Some(cell) = edit
                .map(|edit| edit.current_cell(slot, token))
                .transpose()?
                .flatten()
            {
                cell.is_some()
            } else if let Some(cell) = self
                .stored_delta(reader, group, slot)?
                .map(|delta| delta.get(token))
                .transpose()?
                .flatten()
            {
                cell.is_some()
            } else {
                self.segment_read(reader, (group, token), slot, |cell| Ok(cell.is_some()))?
            };
            if held {
                keys.push(token);
            }
        }
        Ok(Some(keys))
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

    fn changes_over(
        &self,
        group: u64,
        slot: usize,
        mut changes: Changes,
        values: Changes,
    ) -> Result<Changes, GraphError> {
        for (token, value) in values {
            if self.segment_holds(group, token, slot, value.as_ref())? {
                if let Ok(index) = changes.binary_search_by_key(&token, |(held, _)| *held) {
                    changes.remove(index);
                }
            } else {
                columns::put_change(&mut changes, token, value);
            }
        }
        Ok(changes)
    }

    fn segment_holds(
        &self,
        group: u64,
        token: Token<PropertyKey>,
        slot: usize,
        value: Option<&PropertyValue>,
    ) -> Result<bool, GraphError> {
        self.segment_cell((group, token), slot, |cell| {
            Ok(columns::same_state(cell, value.map(Cell::of)))
        })
    }

    /// What commit writes for the property columns of one group: a delta for each changed node
    /// that differs from the segments, or, once the group's stored deltas and changed nodes
    /// together exceed `MAX_DELTAS`, the segments its deltas and changes touch, rewritten.
    pub(crate) fn column_writes(
        &self,
        group: u64,
        edit: ColumnsEdit,
    ) -> Result<(Vec<ColumnWrite>, usize), GraphError> {
        let (stored, slots, heap) = edit.into_slots();
        let folds = stored + slots.len() > columns::MAX_DELTAS;
        let mut merged: Merged<'_> = Vec::with_capacity(slots.len());
        let mut kept = stored;
        for (slot, edit) in slots {
            let key = columns::delta_key(group, slot);
            let before = match edit.fresh || stored == 0 {
                true => None,
                false => self.transaction.get(Table::NodeColumns, &key)?,
            };
            let changes = match &before {
                Some(bytes) => {
                    kept = kept.checked_sub(1).ok_or(DELTA_UNCOUNTED)?;
                    let changes = delta_changes(&key, bytes)?;
                    SlotChanges::Owned(match folds {
                        true => overlaid(changes, edit.values),
                        false => self.changes_over(group, slot, changes, edit.values)?,
                    })
                }
                None if edit.fresh => match edit.created(&heap) {
                    Some(created) if edit.values.is_empty() => SlotChanges::Packed(created),
                    _ => {
                        let mut values = edit.into_changes(&heap)?;
                        values.retain(|(_, value)| value.is_some());
                        SlotChanges::Owned(values)
                    }
                },
                None if folds => SlotChanges::Owned(edit.values),
                None => SlotChanges::Owned(self.changes_over(
                    group,
                    slot,
                    Changes::new(),
                    edit.values,
                )?),
            };
            kept += usize::from(!changes.is_empty());
            merged.push((slot, (changes, before)));
        }
        if folds {
            return Ok((self.fold(group, merged)?, 0));
        }
        let mut writes = Vec::new();
        for (slot, (changes, before)) in merged {
            let key = columns::delta_key(group, slot);
            if changes.is_empty() {
                if before.is_some() {
                    writes.push(ColumnWrite::Delete(key.to_vec()));
                }
                continue;
            }
            let bytes = match &changes {
                SlotChanges::Packed(created) => columns::encode_created_delta(&key, *created),
                SlotChanges::Owned(changes) => columns::encode_delta(
                    &key,
                    changes
                        .iter()
                        .map(|(token, value)| (*token, value.as_ref().map(Cell::of))),
                ),
            };
            if before.as_ref() != Some(&bytes) {
                writes.push(ColumnWrite::Put(key.to_vec(), bytes));
            }
        }
        Ok((writes, kept))
    }

    fn fold(&self, group: u64, merged: Merged<'_>) -> Result<Vec<ColumnWrite>, GraphError> {
        let mut others: Vec<(usize, Vec<u8>, Vec<u8>)> = Vec::new();
        let mut stored = false;
        self.transaction.visit_prefix(
            Table::NodeColumns,
            &columns::deltas_prefix(group),
            |key, bytes| {
                let ColumnKey::Delta { slot, .. } = columns::parse_key(key)? else {
                    return Err(GraphError::Corrupt(
                        "a property column segment lies among deltas",
                    ));
                };
                stored = true;
                if merged
                    .binary_search_by_key(&slot, |(held, _)| *held)
                    .is_err()
                {
                    others.push((slot, key.to_vec(), bytes.to_vec()));
                }
                Ok::<(), GraphError>(())
            },
        )?;
        let mut columns: Vec<(Token<PropertyKey>, SlotCells<'_>)> = Vec::new();
        let mut cells = Vec::new();
        let mut others = others.iter().peekable();
        let mut merged = merged.iter().peekable();
        loop {
            let slot = match (others.peek(), merged.peek()) {
                (Some((other, ..)), Some((changed, _))) if other < changed => others
                    .next()
                    .map(|(slot, key, bytes)| (*slot, Ok((key, bytes)))),
                (_, Some(_)) => merged
                    .next()
                    .map(|(slot, (changes, _))| (*slot, Err(changes))),
                (Some(_), None) => others
                    .next()
                    .map(|(slot, key, bytes)| (*slot, Ok((key, bytes)))),
                (None, None) => None,
            };
            let Some((slot, source)) = slot else {
                break;
            };
            cells.clear();
            match source {
                Ok((key, bytes)) => {
                    for entry in DeltaRef::parse(key, bytes)?.entries() {
                        cells.push(entry?);
                    }
                }
                Err(changes) => changes.push_cells(&mut cells)?,
            }
            for (token, cell) in cells.drain(..) {
                let index = match columns.binary_search_by_key(&token, |(held, _)| *held) {
                    Ok(index) => index,
                    Err(index) => {
                        columns.insert(index, (token, Vec::new()));
                        index
                    }
                };
                if let Some((_, column)) = columns.get_mut(index) {
                    column.push((slot, cell));
                }
            }
        }
        let mut writes = Vec::new();
        let mut changes: SegmentChanges<'_> = Vec::with_capacity(SEGMENT_SLOTS);
        for (token, column) in &columns {
            for run in column
                .chunk_by(|(left, _), (right, _)| left / SEGMENT_SLOTS == right / SEGMENT_SLOTS)
            {
                let Some((first, _)) = run.first() else {
                    continue;
                };
                changes.clear();
                changes.extend(run.iter().map(|(slot, cell)| (slot % SEGMENT_SLOTS, *cell)));
                self.rewrite_segment(group, *token, first / SEGMENT_SLOTS, &changes, &mut writes)?;
            }
        }
        if stored {
            writes.push(ColumnWrite::DeleteDeltas(group));
        }
        Ok(writes)
    }

    fn rewrite_segment(
        &self,
        group: u64,
        token: Token<PropertyKey>,
        segment: usize,
        changes: &SegmentChanges<'_>,
        writes: &mut Vec<ColumnWrite>,
    ) -> Result<(), GraphError> {
        let first_key = columns::segment_key(group, token, segment, 0);
        let mut stored: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        let split = self
            .transaction
            .get_with(Table::NodeColumns, &first_key, |bytes| {
                let whole = SegmentRef::parse(&first_key, bytes)?.part_of(SEGMENT_SLOTS - 1) == 0;
                stored.push((first_key.to_vec(), bytes.to_vec()));
                Ok::<bool, GraphError>(!whole)
            })?
            .unwrap_or(false);
        if split {
            stored.clear();
            self.transaction.visit_prefix(
                Table::NodeColumns,
                &columns::parts_prefix(group, token, segment),
                |key, bytes| {
                    stored.push((key.to_vec(), bytes.to_vec()));
                    Ok::<(), GraphError>(())
                },
            )?;
        }
        let mut cells = Vec::new();
        for (key, bytes) in &stored {
            for cell in SegmentRef::parse(key, bytes)?.cells() {
                cells.push(cell?);
            }
        }
        let parts = columns::encode_segment(
            group,
            token,
            segment,
            &columns::merged(cells.into_iter().map(Ok), changes)?,
        )?;
        for (key, _) in &stored {
            if !parts.iter().any(|(part, _)| **part == **key) {
                writes.push(ColumnWrite::Delete(key.clone()));
            }
        }
        for (key, bytes) in parts {
            let same = stored
                .iter()
                .any(|(held, before)| **held == *key && *before == bytes);
            if !same {
                writes.push(ColumnWrite::Put(key.to_vec(), bytes));
            }
        }
        Ok(())
    }
}

type SegmentChanges<'a> = Vec<(usize, Option<Cell<'a>>)>;

fn overlaid(mut changes: Changes, values: Changes) -> Changes {
    for (token, value) in values {
        columns::put_change(&mut changes, token, value);
    }
    changes
}

type SlotCells<'a> = Vec<(usize, Option<Cell<'a>>)>;

type Merged<'h> = Vec<(usize, (SlotChanges<'h>, Option<Vec<u8>>))>;

enum SlotChanges<'h> {
    Owned(Changes),
    Packed(DeltaRef<'h>),
}

impl SlotChanges<'_> {
    fn is_empty(&self) -> bool {
        match self {
            SlotChanges::Owned(changes) => changes.is_empty(),
            SlotChanges::Packed(created) => created.is_empty(),
        }
    }

    fn push_cells<'s>(
        &'s self,
        out: &mut Vec<(Token<PropertyKey>, Option<Cell<'s>>)>,
    ) -> Result<(), GraphError> {
        match self {
            SlotChanges::Owned(changes) => out.extend(
                changes
                    .iter()
                    .map(|(token, value)| (*token, value.as_ref().map(Cell::of))),
            ),
            SlotChanges::Packed(created) => {
                for entry in created.entries() {
                    out.push(entry?);
                }
            }
        }
        Ok(())
    }
}

impl<T: WriteTransaction> GraphTx<T> {
    fn columns_edit(&mut self, group: u64) -> Result<&mut ColumnsEdit, GraphError> {
        if !self.columns.contains_key(&group) {
            let cached = self
                .column_deltas
                .as_ref()
                .and_then(|counts| lock(counts).get(&group).copied());
            let deltas = match cached {
                Some(deltas) => deltas,
                None => self.count_deltas(group)?,
            };
            self.columns.insert(group, ColumnsEdit::load(deltas));
        }
        self.columns
            .get_mut(&group)
            .ok_or(GraphError::Corrupt("a property column edit vanished"))
    }

    fn count_deltas(&self, group: u64) -> Result<usize, GraphError> {
        let mut deltas = 0;
        self.transaction.visit_prefix(
            Table::NodeColumns,
            &columns::deltas_prefix(group),
            |key, _| {
                let ColumnKey::Delta { .. } = columns::parse_key(key)? else {
                    return Err(GraphError::Corrupt(
                        "a property column segment lies among deltas",
                    ));
                };
                deltas += 1;
                Ok::<(), GraphError>(())
            },
        )?;
        Ok(deltas)
    }

    /// Sets the value of the property `token` of the node in `slot` of `group`, or removes it
    /// with `None`.
    pub(crate) fn set_column_value(
        &mut self,
        (group, token): ColumnId,
        slot: usize,
        value: Option<PropertyValue>,
    ) -> Result<(), GraphError> {
        self.columns_edit(group)?.set(slot, token, value);
        Ok(())
    }

    /// Records that the transaction created the node in `slot` of `group` with `values`,
    /// ascending by key, so the slot holds no stored value.
    pub(crate) fn create_column_node(
        &mut self,
        group: u64,
        slot: usize,
        values: impl IntoIterator<Item = (Token<PropertyKey>, PropertyValue)>,
    ) -> Result<(), GraphError> {
        self.columns_edit(group)?.create(slot, values)
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
        let mut all = Vec::new();
        for (group, edit) in std::mem::take(&mut self.columns) {
            let (writes, deltas) = self.column_writes(group, edit)?;
            self.own_column_deltas.insert(group, deltas);
            all.extend(writes);
        }
        all.sort_by(|left, right| left.key().cmp(&right.key()));
        for write in all {
            match write {
                ColumnWrite::Put(key, value) => {
                    self.transaction.put(Table::NodeColumns, &key, &value)?;
                }
                ColumnWrite::Delete(key) => self.transaction.delete(Table::NodeColumns, &key)?,
                ColumnWrite::DeleteDeltas(group) => {
                    let (start, end) = columns::deltas_range(group);
                    self.transaction
                        .delete_range(Table::NodeColumns, &start, &end)?;
                }
            }
        }
        Ok(())
    }
}
