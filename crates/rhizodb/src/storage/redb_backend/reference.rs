//! The redb backend as a whole store, which only tests use: as the reference the engine is
//! checked against, and under the graph layer's tests.

use std::cell::OnceCell;
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

use redb::backends::InMemoryBackend;
use self_cell::self_cell;

use super::*;
use crate::storage::Storage;
use crate::storage::WriteTransaction;

type WriteTable<'a> = redb::Table<'a, &'static [u8], &'static [u8]>;

#[derive(Debug)]
pub(crate) struct RedbStorage {
    database: Guarded<Database>,
}

impl RedbStorage {
    /// Opens the database file at `path`. When there is none, or only an empty one, the new
    /// database is written under a staging name next to it, synced, and linked into place, so a
    /// power loss leaves either no database or a whole empty one.
    pub(crate) fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        let path = path.as_ref();
        if needs_creating(path).map_err(StorageError::new)? {
            Self::create_staged(path)?;
        }
        Self::create(|| Database::create(path))
    }

    fn create_staged(path: &Path) -> Result<(), StorageError> {
        let staging = staging_path(path).map_err(StorageError::new)?;
        let deadline = Instant::now() + CREATION_WAIT;
        let mut removed_unreadable = false;
        while needs_creating(path).map_err(StorageError::new)? {
            match Self::create(|| Database::create(&staging)) {
                Ok(staged) => {
                    drop(staged);
                    match publish(&staging, path) {
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                        outcome => return outcome.map_err(StorageError::new),
                    }
                }
                Err(error) if is_already_open(&error) && Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(error) if !removed_unreadable && !is_already_open(&error) => {
                    match fs::remove_file(&staging) {
                        Err(removal) if removal.kind() != io::ErrorKind::NotFound => {
                            return Err(StorageError::new(removal));
                        }
                        _ => removed_unreadable = true,
                    }
                }
                Err(error) => return Err(error),
            }
            if Instant::now() >= deadline {
                return Err(StorageError::new(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "another open kept creating the database file",
                )));
            }
        }
        Ok(())
    }

    pub(crate) fn in_memory() -> Result<Self, StorageError> {
        Self::create(|| Database::builder().create_with_backend(InMemoryBackend::new()))
    }

    pub(crate) fn with_backend(backend: impl redb::StorageBackend) -> Result<Self, StorageError> {
        Self::create(|| Database::builder().create_with_backend(backend))
    }

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

const CREATION_WAIT: Duration = Duration::from_secs(10);

fn needs_creating(path: &Path) -> io::Result<bool> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(metadata.is_file() && metadata.len() == 0),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error),
    }
}

fn staging_path(path: &Path) -> io::Result<PathBuf> {
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "the database path does not name a file",
        )
    })?;
    let mut staged = OsString::from(".");
    staged.push(name);
    staged.push(".creating");
    Ok(path.with_file_name(staged))
}

fn publish(staging: &Path, path: &Path) -> io::Result<()> {
    match fs::hard_link(staging, path) {
        Ok(()) => fs::remove_file(staging)?,
        Err(_) if needs_creating(path)? => fs::rename(staging, path)?,
        Err(_) => fs::remove_file(staging)?,
    }
    sync_directory(path)
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    let directory = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    fs::File::open(directory)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> io::Result<()> {
    Ok(())
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

    fn write_copy(snapshot: &RedbRead, path: &Path) -> Result<(), StorageError> {
        let copy = Self::create(|| Database::create(path))?;
        crate::storage::copy_tables(snapshot, &copy)
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
        self.with_table(table, |table| {
            visit_entries(table, prefix, |key| key.starts_with(prefix), visit)
        })
    }

    fn visit_range<E: From<StorageError>>(
        &self,
        table: Table,
        start: &[u8],
        end: &[u8],
        visit: impl FnMut(&[u8], &[u8]) -> Result<(), E>,
    ) -> Result<(), E> {
        if start >= end {
            return Ok(());
        }
        self.with_table(table, |table| {
            visit_entries(table, start, |key| key < end, visit)
        })
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
