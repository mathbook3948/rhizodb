use std::path::Path;
use std::sync::OnceLock;

use redb::Database;
use redb::ReadOnlyTable;
use redb::ReadableDatabase;
use redb::ReadableTable;
use redb::TableDefinition;
use redb::backends::InMemoryBackend;

use super::Entry;
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
        Ok(RedbWrite { transaction })
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

pub(crate) struct RedbWrite {
    transaction: redb::WriteTransaction,
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

impl ReadTransaction for RedbRead {
    fn get(&self, table: Table, key: &[u8]) -> Result<Option<Vec<u8>>, StorageError> {
        let value = self.table(table)?.get(key).map_err(StorageError::new)?;
        Ok(value.map(|guard| guard.value().to_vec()))
    }

    fn scan_prefix(&self, table: Table, prefix: &[u8]) -> Result<Vec<Entry>, StorageError> {
        collect_prefix(self.table(table)?, prefix)
    }
}

impl ReadTransaction for RedbWrite {
    fn get(&self, table: Table, key: &[u8]) -> Result<Option<Vec<u8>>, StorageError> {
        let table = self
            .transaction
            .open_table(definition(table))
            .map_err(StorageError::new)?;
        let value = table.get(key).map_err(StorageError::new)?;
        Ok(value.map(|guard| guard.value().to_vec()))
    }

    fn scan_prefix(&self, table: Table, prefix: &[u8]) -> Result<Vec<Entry>, StorageError> {
        let table = self
            .transaction
            .open_table(definition(table))
            .map_err(StorageError::new)?;
        collect_prefix(&table, prefix)
    }
}

impl WriteTransaction for RedbWrite {
    fn put(&mut self, table: Table, key: &[u8], value: &[u8]) -> Result<(), StorageError> {
        let mut table = self
            .transaction
            .open_table(definition(table))
            .map_err(StorageError::new)?;
        table.insert(key, value).map_err(StorageError::new)?;
        Ok(())
    }

    fn delete(&mut self, table: Table, key: &[u8]) -> Result<(), StorageError> {
        let mut table = self
            .transaction
            .open_table(definition(table))
            .map_err(StorageError::new)?;
        table.remove(key).map_err(StorageError::new)?;
        Ok(())
    }

    fn commit(self) -> Result<(), StorageError> {
        self.transaction.commit().map_err(StorageError::new)
    }

    fn rollback(self) -> Result<(), StorageError> {
        self.transaction.abort().map_err(StorageError::new)
    }
}

#[cfg(test)]
#[path = "redb_backend.test.rs"]
mod tests;
