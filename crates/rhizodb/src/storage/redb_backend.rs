use std::cell::OnceCell;
use std::cmp::Ordering;
use std::ops::Bound;
use std::path::Path;
use std::sync::OnceLock;

use redb::AccessGuard;
use redb::Database;
use redb::ReadOnlyTable;
use redb::ReadableDatabase;
use redb::ReadableTable;
use redb::TableDefinition;
use redb::backends::InMemoryBackend;
use self_cell::self_cell;

use super::Cursor;
use super::Entry;
use super::Reach;
use super::ReadTransaction;
use super::Storage;
use super::StorageError;
use super::Table;
use super::WriteTransaction;

type RedbTable = TableDefinition<'static, &'static [u8], &'static [u8]>;

type OpenTable = ReadOnlyTable<&'static [u8], &'static [u8]>;

const fn definition(table: Table) -> RedbTable {
    TableDefinition::new(table.name())
}

#[derive(Debug)]
pub(crate) struct RedbStorage {
    database: Database,
}

impl RedbStorage {
    pub(crate) fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        let database = Database::create(path).map_err(StorageError::new)?;
        Self::with_all_tables(database)
    }

    pub(crate) fn in_memory() -> Result<Self, StorageError> {
        let database = Database::builder()
            .create_with_backend(InMemoryBackend::new())
            .map_err(StorageError::new)?;
        Self::with_all_tables(database)
    }

    fn with_all_tables(database: Database) -> Result<Self, StorageError> {
        let read = database.begin_read().map_err(StorageError::new)?;
        let complete = Table::ALL
            .into_iter()
            .all(|table| read.open_table(definition(table)).is_ok());
        drop(read);
        if complete {
            return Ok(Self { database });
        }
        let transaction = database.begin_write().map_err(StorageError::new)?;
        for table in Table::ALL {
            transaction
                .open_table(definition(table))
                .map_err(StorageError::new)?;
        }
        transaction.commit().map_err(StorageError::new)?;
        Ok(Self { database })
    }
}

impl Storage for RedbStorage {
    type Read<'a> = RedbRead;
    type Write<'a> = RedbWrite;

    fn begin_read(&self) -> Result<RedbRead, StorageError> {
        let transaction = self.database.begin_read().map_err(StorageError::new)?;
        Ok(RedbRead {
            transaction,
            tables: std::array::from_fn(|_| OnceLock::new()),
        })
    }

    fn begin_write(&self) -> Result<RedbWrite, StorageError> {
        let transaction = self.database.begin_write().map_err(StorageError::new)?;
        Ok(RedbWrite {
            cell: WriteCell::new(transaction, |_| std::array::from_fn(|_| OnceCell::new())),
        })
    }
}

/// A read transaction that opens each table once, on its first use.
#[derive(Debug)]
pub(crate) struct RedbRead {
    transaction: redb::ReadTransaction,
    tables: [OnceLock<OpenTable>; Table::ALL.len()],
}

impl RedbRead {
    fn table(&self, table: Table) -> Result<&OpenTable, StorageError> {
        let cell = &self.tables[table as usize];
        if let Some(open) = cell.get() {
            return Ok(open);
        }
        let open = self
            .transaction
            .open_table(definition(table))
            .map_err(StorageError::new)?;
        Ok(cell.get_or_init(|| open))
    }
}

type WriteTables<'a> = [OnceCell<redb::Table<'a, &'static [u8], &'static [u8]>>; Table::ALL.len()];

self_cell!(
    struct WriteCell {
        owner: redb::WriteTransaction,
        #[not_covariant]
        dependent: WriteTables,
    }
);

/// A write transaction that opens each table once, on its first use, and keeps it open until
/// the transaction ends.
pub(crate) struct RedbWrite {
    cell: WriteCell,
}

fn open_write<'t, 'a>(
    transaction: &'a redb::WriteTransaction,
    tables: &'t WriteTables<'a>,
    table: Table,
) -> Result<&'t redb::Table<'a, &'static [u8], &'static [u8]>, StorageError> {
    let cell = &tables[table as usize];
    if let Some(open) = cell.get() {
        return Ok(open);
    }
    let open = transaction
        .open_table(definition(table))
        .map_err(StorageError::new)?;
    Ok(cell.get_or_init(|| open))
}

impl RedbWrite {
    fn with_table<R, E: From<StorageError>>(
        &self,
        table: Table,
        work: impl FnOnce(&redb::Table<'_, &'static [u8], &'static [u8]>) -> Result<R, E>,
    ) -> Result<R, E> {
        self.cell
            .with_dependent(|transaction, tables| work(open_write(transaction, tables, table)?))
    }

    fn with_table_mut<R>(
        &mut self,
        table: Table,
        work: impl FnOnce(&mut redb::Table<'_, &'static [u8], &'static [u8]>) -> Result<R, StorageError>,
    ) -> Result<R, StorageError> {
        self.cell.with_dependent_mut(|transaction, tables| {
            let slot = &mut tables[table as usize];
            let mut open = match std::mem::take(slot).into_inner() {
                Some(open) => open,
                None => transaction
                    .open_table(definition(table))
                    .map_err(StorageError::new)?,
            };
            let result = work(&mut open);
            *slot = OnceCell::from(open);
            result
        })
    }
}

fn collect_prefix<T: ReadableTable<&'static [u8], &'static [u8]>>(
    table: &T,
    prefix: &[u8],
) -> Result<Vec<Entry>, StorageError> {
    let mut entries = Vec::new();
    for item in table.range(prefix..).map_err(StorageError::new)? {
        let (key, value) = item.map_err(StorageError::new)?;
        if !key.value().starts_with(prefix) {
            break;
        }
        entries.push((key.value().to_vec(), value.value().to_vec()));
    }
    Ok(entries)
}

fn read_value<T: ReadableTable<&'static [u8], &'static [u8]>, R, E: From<StorageError>>(
    table: &T,
    key: &[u8],
    read: impl FnOnce(&[u8]) -> Result<R, E>,
) -> Result<Option<R>, E> {
    match table.get(key).map_err(StorageError::new)? {
        Some(guard) => read(guard.value()).map(Some),
        None => Ok(None),
    }
}

fn visit_entries<T: ReadableTable<&'static [u8], &'static [u8]>, E: From<StorageError>>(
    table: &T,
    prefix: &[u8],
    mut visit: impl FnMut(&[u8], &[u8]) -> Result<(), E>,
) -> Result<(), E> {
    for item in table.range(prefix..).map_err(StorageError::new)? {
        let (key, value) = item.map_err(StorageError::new)?;
        if !key.value().starts_with(prefix) {
            break;
        }
        visit(key.value(), value.value())?;
    }
    Ok(())
}

fn collect_range<T: ReadableTable<&'static [u8], &'static [u8]>>(
    table: &T,
    start: &[u8],
    end: &[u8],
) -> Result<Vec<Entry>, StorageError> {
    let mut entries = Vec::new();
    if start >= end {
        return Ok(entries);
    }
    for item in table.range(start..end).map_err(StorageError::new)? {
        let (key, value) = item.map_err(StorageError::new)?;
        entries.push((key.value().to_vec(), value.value().to_vec()));
    }
    Ok(entries)
}

const MAX_STEPS: usize = 8;

type Guard<'a> = AccessGuard<'a, &'static [u8]>;

/// A read cursor that keeps its place and the two values it read last.
pub(crate) struct ReadCursor<'a> {
    table: &'a OpenTable,
    place: Option<Place<'a>>,
    recent: [Option<(Vec<u8>, Option<Guard<'a>>)>; 2],
    older: usize,
}

struct Place<'a> {
    cursor: redb::Cursor<'a, &'static [u8], &'static [u8]>,
    reached: Vec<u8>,
    ahead: Option<(Guard<'a>, Guard<'a>)>,
}

impl<'a> Place<'a> {
    fn step_to(
        &mut self,
        key: &[u8],
        steps: usize,
    ) -> Result<Option<Option<Guard<'a>>>, StorageError> {
        for _ in 0..=steps {
            let entry = match self.ahead.take() {
                Some(entry) => Some(entry),
                None => self.cursor.next().map_err(StorageError::new)?,
            };
            let (ordering, value) = match entry {
                None => (Ordering::Greater, None),
                Some((found, value)) => match found.value().cmp(key) {
                    Ordering::Less => continue,
                    Ordering::Equal => (Ordering::Equal, Some(value)),
                    Ordering::Greater => {
                        self.ahead = Some((found, value));
                        (Ordering::Greater, None)
                    }
                },
            };
            self.reached.clear();
            self.reached.extend_from_slice(key);
            return Ok(Some(value.filter(|_| ordering == Ordering::Equal)));
        }
        Ok(None)
    }
}

impl<'a> ReadCursor<'a> {
    fn seek(&mut self, key: &[u8]) -> Result<Option<Guard<'a>>, StorageError> {
        let cursor = self
            .table
            .lower_bound(Bound::Included(key))
            .map_err(StorageError::new)?;
        let place = self.place.insert(Place {
            cursor,
            reached: Vec::new(),
            ahead: None,
        });
        Ok(place.step_to(key, 0)?.flatten())
    }

    fn find(&mut self, key: &[u8], reach: Reach) -> Result<Option<Guard<'a>>, StorageError> {
        match (reach, &mut self.place) {
            (Reach::Step, Some(place)) if key > place.reached.as_slice() => {
                match place.step_to(key, MAX_STEPS)? {
                    Some(found) => Ok(found),
                    None => self.seek(key),
                }
            }
            (Reach::Lookup, Some(_)) => self.table.get(key).map_err(StorageError::new),
            _ => self.seek(key),
        }
    }
}

impl Cursor for ReadCursor<'_> {
    fn get_with<R, E: From<StorageError>>(
        &mut self,
        key: &[u8],
        reach: Reach,
        read: impl FnOnce(&[u8]) -> Result<R, E>,
    ) -> Result<Option<R>, E> {
        let holds = |entry: &Option<(Vec<u8>, Option<Guard<'_>>)>| matches!(entry, Some((held, _)) if held == key);
        let newer = 1 - self.older;
        let slot = if holds(&self.recent[newer]) {
            newer
        } else if holds(&self.recent[self.older]) {
            self.older
        } else {
            let slot = self.older;
            let found = self.find(key, reach)?;
            let (held, value) = self.recent[slot].get_or_insert_with(|| (Vec::new(), None));
            held.clear();
            held.extend_from_slice(key);
            *value = found;
            slot
        };
        self.older = 1 - slot;
        match &self.recent[slot] {
            Some((_, Some(value))) => read(value.value()).map(Some),
            _ => Ok(None),
        }
    }
}

/// A write transaction's cursor, which looks each key up on its own.
pub(crate) struct WriteCursor<'a> {
    transaction: &'a RedbWrite,
    table: Table,
}

impl Cursor for WriteCursor<'_> {
    fn get_with<R, E: From<StorageError>>(
        &mut self,
        key: &[u8],
        _reach: Reach,
        read: impl FnOnce(&[u8]) -> Result<R, E>,
    ) -> Result<Option<R>, E> {
        self.transaction.get_with(self.table, key, read)
    }
}

impl ReadTransaction for RedbRead {
    type Cursor<'a> = ReadCursor<'a>;

    fn cursor(&self, table: Table) -> Result<ReadCursor<'_>, StorageError> {
        Ok(ReadCursor {
            table: self.table(table)?,
            place: None,
            recent: [None, None],
            older: 0,
        })
    }

    fn get(&self, table: Table, key: &[u8]) -> Result<Option<Vec<u8>>, StorageError> {
        let value = self.table(table)?.get(key).map_err(StorageError::new)?;
        Ok(value.map(|guard| guard.value().to_vec()))
    }

    fn scan_prefix(&self, table: Table, prefix: &[u8]) -> Result<Vec<Entry>, StorageError> {
        collect_prefix(self.table(table)?, prefix)
    }

    fn scan_range(
        &self,
        table: Table,
        start: &[u8],
        end: &[u8],
    ) -> Result<Vec<Entry>, StorageError> {
        collect_range(self.table(table)?, start, end)
    }

    fn get_with<R, E: From<StorageError>>(
        &self,
        table: Table,
        key: &[u8],
        read: impl FnOnce(&[u8]) -> Result<R, E>,
    ) -> Result<Option<R>, E> {
        read_value(self.table(table)?, key, read)
    }

    fn visit_prefix<E: From<StorageError>>(
        &self,
        table: Table,
        prefix: &[u8],
        visit: impl FnMut(&[u8], &[u8]) -> Result<(), E>,
    ) -> Result<(), E> {
        visit_entries(self.table(table)?, prefix, visit)
    }
}

impl ReadTransaction for RedbWrite {
    type Cursor<'a> = WriteCursor<'a>;

    fn cursor(&self, table: Table) -> Result<WriteCursor<'_>, StorageError> {
        Ok(WriteCursor {
            transaction: self,
            table,
        })
    }

    fn get(&self, table: Table, key: &[u8]) -> Result<Option<Vec<u8>>, StorageError> {
        self.with_table(table, |table| {
            let value = table.get(key).map_err(StorageError::new)?;
            Ok(value.map(|guard| guard.value().to_vec()))
        })
    }

    fn scan_prefix(&self, table: Table, prefix: &[u8]) -> Result<Vec<Entry>, StorageError> {
        self.with_table(table, |table| collect_prefix(table, prefix))
    }

    fn scan_range(
        &self,
        table: Table,
        start: &[u8],
        end: &[u8],
    ) -> Result<Vec<Entry>, StorageError> {
        self.with_table(table, |table| collect_range(table, start, end))
    }

    fn get_with<R, E: From<StorageError>>(
        &self,
        table: Table,
        key: &[u8],
        read: impl FnOnce(&[u8]) -> Result<R, E>,
    ) -> Result<Option<R>, E> {
        self.with_table(table, |table| read_value(table, key, read))
    }

    fn visit_prefix<E: From<StorageError>>(
        &self,
        table: Table,
        prefix: &[u8],
        visit: impl FnMut(&[u8], &[u8]) -> Result<(), E>,
    ) -> Result<(), E> {
        self.with_table(table, |table| visit_entries(table, prefix, visit))
    }
}

impl WriteTransaction for RedbWrite {
    fn put(&mut self, table: Table, key: &[u8], value: &[u8]) -> Result<(), StorageError> {
        self.with_table_mut(table, |table| {
            table.insert(key, value).map_err(StorageError::new)?;
            Ok(())
        })
    }

    fn delete(&mut self, table: Table, key: &[u8]) -> Result<(), StorageError> {
        self.with_table_mut(table, |table| {
            table.remove(key).map_err(StorageError::new)?;
            Ok(())
        })
    }

    fn commit(self) -> Result<(), StorageError> {
        self.cell.into_owner().commit().map_err(StorageError::new)
    }

    fn rollback(self) -> Result<(), StorageError> {
        self.cell.into_owner().abort().map_err(StorageError::new)
    }
}

#[cfg(test)]
#[path = "redb_backend.test.rs"]
mod tests;
