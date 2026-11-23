use std::any::Any;
use std::cell::OnceCell;
use std::error::Error;
use std::fmt;
use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic;
use std::sync::atomic::AtomicBool;

use redb::AccessGuard;
use redb::Database;
use redb::ReadOnlyTable;
use redb::ReadableDatabase;
use redb::ReadableTable;
use redb::TableDefinition;
use redb::backends::InMemoryBackend;
use self_cell::self_cell;

use super::Entry;
use super::ReadTransaction;
use super::Storage;
use super::StorageError;
use super::Table;
use super::WriteTransaction;

type RedbTable = TableDefinition<'static, &'static [u8], &'static [u8]>;

type OpenTable = ReadOnlyTable<&'static [u8], &'static [u8]>;

type WriteTable<'a> = redb::Table<'a, &'static [u8], &'static [u8]>;

type Guard<'a> = AccessGuard<'a, &'static [u8]>;

type Pair<'a> = (&'a [u8], &'a [u8]);

const fn definition(table: Table) -> RedbTable {
    TableDefinition::new(table.name())
}

fn redb_error(error: impl Into<redb::Error>) -> StorageError {
    match error.into() {
        error @ redb::Error::Corrupted(_) => StorageError::corrupt(error),
        error => StorageError::new(error),
    }
}

/// redb panicked, most likely on a damaged file, and the guard returned this in its place.
#[derive(Debug)]
pub(crate) struct PanicError {
    message: String,
}

impl PanicError {
    fn new(payload: &(dyn Any + Send)) -> Self {
        let message = payload
            .downcast_ref::<&str>()
            .map(|message| (*message).to_owned())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_default();
        Self { message }
    }
}

impl fmt::Display for PanicError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "the storage engine panicked, most likely on a damaged file: {}",
            self.message
        )
    }
}

impl Error for PanicError {}

/// An operation on a database whose storage engine panicked earlier.
#[derive(Debug)]
pub(crate) struct PoisonedError;

impl fmt::Display for PoisonedError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(
            "the storage engine panicked earlier, so the database refuses every operation until \
             the file is opened again",
        )
    }
}

impl Error for PoisonedError {}

/// Set once redb panics. Every later call on the database then fails, because redb's state may
/// no longer match the file.
#[derive(Clone, Debug, Default)]
pub(crate) struct Poison(Arc<AtomicBool>);

impl Poison {
    pub(crate) fn is_set(&self) -> bool {
        self.0.load(atomic::Ordering::Acquire)
    }

    fn catch<R>(&self, call: impl FnOnce() -> R) -> Result<R, StorageError> {
        std::panic::catch_unwind(AssertUnwindSafe(call)).map_err(|payload| {
            self.0.store(true, atomic::Ordering::Release);
            StorageError::corrupt(PanicError::new(payload.as_ref()))
        })
    }

    /// Runs `call`, which calls redb and no code outside it, and returns a panic in it as a
    /// corruption error that poisons the database.
    pub(crate) fn guard<R>(&self, call: impl FnOnce() -> R) -> Result<R, StorageError> {
        if self.is_set() {
            return Err(StorageError::corrupt(PoisonedError));
        }
        self.catch(call)
    }

    /// Drops a redb value under the guard. Once the database is poisoned, the value is dropped
    /// while an unwind is in progress, so redb takes the path it takes for its own panic: it
    /// writes nothing more and leaves the file to the recovery of the next open.
    pub(crate) fn release<T>(&self, value: T) {
        if self.is_set() {
            let _ = std::panic::catch_unwind(AssertUnwindSafe(move || {
                let _unwinding = DroppedInUnwind(Some(value));
                std::panic::resume_unwind(Box::new(PoisonedError))
            }));
        } else {
            let _ = self.catch(move || drop(value));
        }
    }

    /// Runs `work` on a redb value and then releases the value.
    pub(crate) fn with_released<T, R>(&self, mut value: T, work: impl FnOnce(&mut T) -> R) -> R {
        let result = work(&mut value);
        self.release(value);
        result
    }
}

/// A value dropped during an unwind, whose own panic is caught inside its destructor, since a
/// panic that left a destructor during an unwind would abort the process.
pub(crate) struct DroppedInUnwind<T>(Option<T>);

impl<T> Drop for DroppedInUnwind<T> {
    fn drop(&mut self) {
        let value = self.0.take();
        let _ = std::panic::catch_unwind(AssertUnwindSafe(move || drop(value)));
    }
}

/// A redb value that is reached only through the panic guard and dropped under it.
#[derive(Debug)]
pub(crate) struct Guarded<T> {
    value: Option<T>,
    poison: Poison,
}

impl<T> Guarded<T> {
    fn new(value: T, poison: &Poison) -> Self {
        Self {
            value: Some(value),
            poison: poison.clone(),
        }
    }

    fn poison(&self) -> &Poison {
        &self.poison
    }

    fn call<'s, R>(&'s self, call: impl FnOnce(&'s T) -> R) -> Result<R, StorageError> {
        match &self.value {
            Some(value) => self.poison.guard(|| call(value)),
            None => Err(StorageError::corrupt(PoisonedError)),
        }
    }

    fn call_mut<R>(&mut self, call: impl FnOnce(&mut T) -> R) -> Result<R, StorageError> {
        match &mut self.value {
            Some(value) => self.poison.guard(|| call(value)),
            None => Err(StorageError::corrupt(PoisonedError)),
        }
    }

    fn consume<R>(mut self, call: impl FnOnce(T) -> R) -> Result<R, StorageError> {
        let Some(value) = self.value.take() else {
            return Err(StorageError::corrupt(PoisonedError));
        };
        if self.poison.is_set() {
            self.poison.release(value);
            return Err(StorageError::corrupt(PoisonedError));
        }
        self.poison.catch(move || call(value))
    }
}

impl<T> Drop for Guarded<T> {
    fn drop(&mut self) {
        if let Some(value) = self.value.take() {
            self.poison.release(value);
        }
    }
}

#[derive(Debug)]
pub(crate) struct RedbStorage {
    database: Guarded<Database>,
}

impl RedbStorage {
    pub(crate) fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        Self::create(|| Database::create(path))
    }

    pub(crate) fn in_memory() -> Result<Self, StorageError> {
        Self::create(|| Database::builder().create_with_backend(InMemoryBackend::new()))
    }

    #[cfg(test)]
    pub(crate) fn with_backend(backend: impl redb::StorageBackend) -> Result<Self, StorageError> {
        Self::create(|| Database::builder().create_with_backend(backend))
    }

    #[cfg(test)]
    pub(crate) fn with_uncached_backend(
        backend: impl redb::StorageBackend,
    ) -> Result<Self, StorageError> {
        Self::create(|| {
            Database::builder()
                .set_cache_size(0)
                .create_with_backend(backend)
        })
    }

    fn create(
        create: impl FnOnce() -> Result<Database, redb::DatabaseError>,
    ) -> Result<Self, StorageError> {
        let poison = Poison::default();
        let database = Guarded::new(poison.guard(create)?.map_err(redb_error)?, &poison);
        let complete = database
            .call(|database| -> Result<bool, redb::Error> {
                let read = database.begin_read()?;
                Ok(Table::ALL
                    .into_iter()
                    .filter(|table| !table.is_legacy())
                    .all(|table| read.open_table(definition(table)).is_ok()))
            })?
            .map_err(redb_error)?;
        if !complete {
            database
                .call(|database| -> Result<(), redb::Error> {
                    let transaction = database.begin_write()?;
                    for table in Table::ALL.into_iter().filter(|table| !table.is_legacy()) {
                        transaction.open_table(definition(table))?;
                    }
                    transaction.commit()?;
                    Ok(())
                })?
                .map_err(redb_error)?;
        }
        Ok(Self { database })
    }
}

impl Storage for RedbStorage {
    type Read<'a> = RedbRead;
    type Write<'a> = RedbWrite;

    fn begin_read(&self) -> Result<RedbRead, StorageError> {
        let transaction = self
            .database
            .call(|database| database.begin_read())?
            .map_err(redb_error)?;
        Ok(RedbRead {
            tables: std::array::from_fn(|_| OnceLock::new()),
            transaction: Guarded::new(transaction, self.database.poison()),
        })
    }

    fn begin_write(&self) -> Result<RedbWrite, StorageError> {
        let transaction = self
            .database
            .call(|database| database.begin_write())?
            .map_err(redb_error)?;
        Ok(RedbWrite {
            cell: WriteCell::new(Guarded::new(transaction, self.database.poison()), |_| {
                std::array::from_fn(|_| OnceCell::new())
            }),
        })
    }
}

/// A read transaction that opens each table once, on its first use.
#[derive(Debug)]
pub(crate) struct RedbRead {
    tables: [OnceLock<Guarded<OpenTable>>; Table::ALL.len()],
    transaction: Guarded<redb::ReadTransaction>,
}

impl RedbRead {
    fn table(&self, table: Table) -> Result<&Guarded<OpenTable>, StorageError> {
        let cell = &self.tables[table as usize];
        if let Some(open) = cell.get() {
            return Ok(open);
        }
        let open = self
            .transaction
            .call(|transaction| transaction.open_table(definition(table)))?
            .map_err(redb_error)?;
        let open = Guarded::new(open, self.transaction.poison());
        Ok(cell.get_or_init(|| open))
    }
}

type WriteTables<'a> = [OnceCell<Guarded<WriteTable<'a>>>; Table::ALL.len()];

self_cell!(
    struct WriteCell {
        owner: Guarded<redb::WriteTransaction>,
        #[not_covariant]
        dependent: WriteTables,
    }
);

/// A write transaction that opens each table once, on its first use, and keeps it open until
/// the transaction ends.
pub(crate) struct RedbWrite {
    cell: WriteCell,
}

fn open_write_table<'a>(
    transaction: &'a Guarded<redb::WriteTransaction>,
    table: Table,
) -> Result<Guarded<WriteTable<'a>>, StorageError> {
    let open = transaction
        .call(|transaction| transaction.open_table(definition(table)))?
        .map_err(redb_error)?;
    Ok(Guarded::new(open, transaction.poison()))
}

impl RedbWrite {
    fn with_table<R, E: From<StorageError>>(
        &self,
        table: Table,
        work: impl FnOnce(&Guarded<WriteTable<'_>>) -> Result<R, E>,
    ) -> Result<R, E> {
        self.cell.with_dependent(|transaction, tables| {
            let cell = &tables[table as usize];
            let open = match cell.get() {
                Some(open) => open,
                None => {
                    let open = open_write_table(transaction, table)?;
                    cell.get_or_init(|| open)
                }
            };
            work(open)
        })
    }

    fn with_table_mut<R>(
        &mut self,
        table: Table,
        work: impl FnOnce(&mut WriteTable<'_>) -> Result<R, redb::StorageError>,
    ) -> Result<R, StorageError> {
        self.cell.with_dependent_mut(|transaction, tables| {
            let slot = &mut tables[table as usize];
            let mut open = match std::mem::take(slot).into_inner() {
                Some(open) => open,
                None => open_write_table(transaction, table)?,
            };
            let result = open.call_mut(work);
            *slot = OnceCell::from(open);
            result?.map_err(redb_error)
        })
    }
}

fn collect_prefix<T: ReadableTable<&'static [u8], &'static [u8]>>(
    table: &Guarded<T>,
    prefix: &[u8],
) -> Result<Vec<Entry>, StorageError> {
    table
        .call(|table| -> Result<Vec<Entry>, redb::StorageError> {
            let mut entries = Vec::new();
            for item in table.range(prefix..)? {
                let (key, value) = item?;
                if !key.value().starts_with(prefix) {
                    break;
                }
                entries.push((key.value().to_vec(), value.value().to_vec()));
            }
            Ok(entries)
        })?
        .map_err(redb_error)
}

fn collect_range<T: ReadableTable<&'static [u8], &'static [u8]>>(
    table: &Guarded<T>,
    start: &[u8],
    end: &[u8],
) -> Result<Vec<Entry>, StorageError> {
    if start >= end {
        return Ok(Vec::new());
    }
    table
        .call(|table| -> Result<Vec<Entry>, redb::StorageError> {
            table
                .range(start..end)?
                .map(|item| {
                    let (key, value) = item?;
                    Ok((key.value().to_vec(), value.value().to_vec()))
                })
                .collect()
        })?
        .map_err(redb_error)
}

fn read_copy<T: ReadableTable<&'static [u8], &'static [u8]>>(
    table: &Guarded<T>,
    key: &[u8],
) -> Result<Option<Vec<u8>>, StorageError> {
    table
        .call(|table| -> Result<Option<Vec<u8>>, redb::StorageError> {
            Ok(table.get(key)?.map(|guard| guard.value().to_vec()))
        })?
        .map_err(redb_error)
}

fn read_value<T: ReadableTable<&'static [u8], &'static [u8]>, R, E: From<StorageError>>(
    table: &Guarded<T>,
    key: &[u8],
    read: impl FnOnce(&[u8]) -> Result<R, E>,
) -> Result<Option<R>, E> {
    let poison = table.poison();
    let Some(guard) = table.call(|table| table.get(key))?.map_err(redb_error)? else {
        return Ok(None);
    };
    poison.with_released(guard, |guard| {
        let value = poison.guard(|| guard.value())?;
        read(value).map(Some)
    })
}

fn visit_entries<T: ReadableTable<&'static [u8], &'static [u8]>, E: From<StorageError>>(
    table: &Guarded<T>,
    prefix: &[u8],
    mut visit: impl FnMut(&[u8], &[u8]) -> Result<(), E>,
) -> Result<(), E> {
    let poison = table.poison();
    let range = table
        .call(|table| table.range(prefix..))?
        .map_err(redb_error)?;
    let held: Option<(Guard<'_>, Guard<'_>)> = None;
    poison.with_released((range, held), |(range, held)| {
        loop {
            let entry = poison
                .guard(|| -> Result<Option<Pair<'_>>, redb::StorageError> {
                    *held = range.next().transpose()?;
                    Ok(held
                        .as_ref()
                        .map(|(key, value)| (key.value(), value.value()))
                        .filter(|(key, _)| key.starts_with(prefix)))
                })?
                .map_err(redb_error)?;
            match entry {
                Some((key, value)) => visit(key, value)?,
                None => return Ok(()),
            }
        }
    })
}

impl ReadTransaction for RedbRead {
    fn get(&self, table: Table, key: &[u8]) -> Result<Option<Vec<u8>>, StorageError> {
        read_copy(self.table(table)?, key)
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
    fn get(&self, table: Table, key: &[u8]) -> Result<Option<Vec<u8>>, StorageError> {
        self.with_table(table, |table| read_copy(table, key))
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
        self.with_table_mut(table, |table| table.insert(key, value).map(drop))
    }

    fn delete(&mut self, table: Table, key: &[u8]) -> Result<(), StorageError> {
        self.with_table_mut(table, |table| table.remove(key).map(drop))
    }

    fn delete_range(&mut self, table: Table, start: &[u8], end: &[u8]) -> Result<(), StorageError> {
        if start >= end {
            return Ok(());
        }
        self.with_table_mut(table, |table| table.retain_in(start..end, |_, _| false))
    }

    fn commit(self) -> Result<(), StorageError> {
        self.cell
            .into_owner()
            .consume(redb::WriteTransaction::commit)?
            .map_err(redb_error)
    }

    fn rollback(self) -> Result<(), StorageError> {
        self.cell
            .into_owner()
            .consume(redb::WriteTransaction::abort)?
            .map_err(redb_error)
    }
}

#[cfg(test)]
#[path = "redb_backend.test.rs"]
mod tests;
