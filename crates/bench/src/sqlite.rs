use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;

use rusqlite::Connection;
use rusqlite::Statement;
use rusqlite::types::Value as Sql;
use rusqlite::types::ValueRef;

use crate::data::Dataset;
use crate::engine::Built;
use crate::engine::Dialect;
use crate::engine::Engine;
use crate::engine::EngineError;
use crate::engine::EngineKind;
use crate::engine::Instance;
use crate::engine::Param;
use crate::engine::Query;
use crate::engine::Session;
use crate::fingerprint::Fingerprint;
use crate::fingerprint::RowHasher;

const FILE_NAME: &str = "sqlite.db";
const CACHE_KIB: i64 = 512 * 1024;

const SCHEMA: &str = "
CREATE TABLE persons(id INTEGER PRIMARY KEY, name TEXT, age INTEGER, bio TEXT, embedding BLOB);
CREATE TABLE cities(id INTEGER PRIMARY KEY, name TEXT);
CREATE TABLE knows(src INTEGER NOT NULL, dst INTEGER NOT NULL, since INTEGER);
CREATE TABLE lives_in(src INTEGER PRIMARY KEY, dst INTEGER NOT NULL);
CREATE INDEX knows_by_src ON knows(src, dst);
CREATE INDEX knows_by_dst ON knows(dst, src);
";
const DROP_INDEXES: &str = "DROP INDEX knows_by_src; DROP INDEX knows_by_dst;";
const CREATE_INDEXES: &str = "
CREATE INDEX knows_by_src ON knows(src, dst);
CREATE INDEX knows_by_dst ON knows(dst, src);
";

/// SQLite through `rusqlite`, bundled and built from source, with one file in write-ahead log
/// mode. Every commit is durable, since `synchronous` stays at its default.
#[derive(Clone, Copy, Debug)]
pub struct SqliteEngine;

type Blobs = Vec<Vec<u8>>;

struct SqliteInstance {
    path: PathBuf,
    primary: Mutex<Option<Connection>>,
    blobs: Mutex<Option<Blobs>>,
}

impl std::fmt::Debug for SqliteInstance {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SqliteInstance")
            .finish_non_exhaustive()
    }
}

struct SqliteSession {
    connection: Connection,
}

fn connect(path: &Path) -> Result<Connection, EngineError> {
    let connection = Connection::open(path).map_err(failed)?;
    connection
        .pragma_update(None, "journal_mode", "WAL")
        .map_err(failed)?;
    connection
        .pragma_update(None, "cache_size", -CACHE_KIB)
        .map_err(failed)?;
    connection
        .pragma_update(None, "temp_store", "MEMORY")
        .map_err(failed)?;
    Ok(connection)
}

fn failed(error: rusqlite::Error) -> EngineError {
    EngineError::Failed(error.to_string())
}

fn blob(values: &[f64]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|&value| (value as f32).to_le_bytes())
        .collect()
}

impl Engine for SqliteEngine {
    fn kind(&self) -> EngineKind {
        EngineKind::Sqlite
    }

    fn dialect(&self) -> Dialect {
        Dialect::Sql
    }

    fn open(&self, dir: &Path) -> Result<Box<dyn Instance>, EngineError> {
        let path = dir.join(FILE_NAME);
        let primary = connect(&path)?;
        Ok(Box::new(SqliteInstance {
            path,
            primary: Mutex::new(Some(primary)),
            blobs: Mutex::new(None),
        }))
    }

    fn config(&self) -> serde_json::Value {
        serde_json::json!({
            "version": rusqlite::version(),
            "pinned": env!("PIN_RUSQLITE"),
            "storage": "file",
            "journal_mode": "WAL",
            "synchronous": "FULL, the default, so every commit is durable",
            "cache_size_kib": CACHE_KIB,
            "temp_store": "MEMORY",
            "threads": "one connection per thread",
            "indexes": "knows(src, dst), knows(dst, src), primary keys on every id",
            "keyword_search": "FTS5, unicode61 tokenizer",
            "vector_search": "unsupported",
        })
    }
}

impl SqliteInstance {
    fn with<T>(
        &self,
        work: impl FnOnce(&mut Connection) -> Result<T, rusqlite::Error>,
    ) -> Result<T, EngineError> {
        let mut guard = self.primary.lock().map_err(|_| {
            EngineError::Failed("the primary connection lock is poisoned".to_owned())
        })?;
        let connection = guard
            .as_mut()
            .ok_or_else(|| EngineError::Failed("the database is closed".to_owned()))?;
        work(connection).map_err(failed)
    }
}

impl Instance for SqliteInstance {
    fn session(&self) -> Result<Box<dyn Session + '_>, EngineError> {
        Ok(Box::new(SqliteSession {
            connection: connect(&self.path)?,
        }))
    }

    fn create_schema(&self) -> Result<(), EngineError> {
        self.with(|connection| connection.execute_batch(SCHEMA))
    }

    fn prepare_bulk_load(&self, data: &Dataset, _scratch: &Path) -> Result<(), EngineError> {
        let blobs = data
            .people
            .iter()
            .map(|person| blob(&person.embedding))
            .collect();
        *self
            .blobs
            .lock()
            .map_err(|_| EngineError::Failed("the prepared input lock is poisoned".to_owned()))? =
            Some(blobs);
        Ok(())
    }

    fn close(self: Box<Self>) -> Result<(), EngineError> {
        let connection = self
            .primary
            .lock()
            .map_err(|_| EngineError::Failed("the primary connection lock is poisoned".to_owned()))?
            .take();
        match connection {
            Some(connection) => connection.close().map_err(|(_, error)| failed(error)),
            None => Ok(()),
        }
    }

    fn bulk_load(&self, data: &Dataset, _scratch: &Path) -> Result<(), EngineError> {
        let blobs = self
            .blobs
            .lock()
            .map_err(|_| EngineError::Failed("the prepared input lock is poisoned".to_owned()))?
            .take()
            .ok_or_else(|| EngineError::Failed("prepare_bulk_load was not called".to_owned()))?;
        self.with(|connection| {
            let transaction = connection.transaction()?;
            transaction.execute_batch(DROP_INDEXES)?;
            {
                let mut insert = transaction.prepare(
                    "INSERT INTO persons(id, name, age, bio, embedding) VALUES (?1, ?2, ?3, ?4, ?5)",
                )?;
                for (person, blob) in data.people.iter().zip(&blobs) {
                    insert.execute(rusqlite::params![
                        person.id,
                        person.name,
                        person.age,
                        person.bio,
                        blob
                    ])?;
                }
                let mut insert = transaction.prepare("INSERT INTO cities(id, name) VALUES (?1, ?2)")?;
                for city in &data.cities {
                    insert.execute(rusqlite::params![city.id, city.name])?;
                }
                let mut insert =
                    transaction.prepare("INSERT INTO knows(src, dst, since) VALUES (?1, ?2, ?3)")?;
                for edge in &data.knows {
                    insert.execute(rusqlite::params![edge.from, edge.to, edge.since])?;
                }
                let mut insert = transaction.prepare("INSERT INTO lives_in(src, dst) VALUES (?1, ?2)")?;
                for edge in &data.lives_in {
                    insert.execute(rusqlite::params![edge.person, edge.city])?;
                }
            }
            transaction.execute_batch(CREATE_INDEXES)?;
            transaction.commit()
        })
    }

    fn create_keyword_index(&self) -> Result<Built, EngineError> {
        self.with(|connection| {
            connection.execute_batch(
                "CREATE VIRTUAL TABLE bio_fts USING fts5(bio, content='persons', content_rowid='id');
                 INSERT INTO bio_fts(bio_fts) VALUES('rebuild');",
            )
        })?;
        Ok(Built::Index)
    }

    fn create_vector_index(&self) -> Result<Built, EngineError> {
        Err(EngineError::Unsupported(
            "SQLite has no vector index or distance function, and a scan in SQL over blobs is not a vector search"
                .to_owned(),
        ))
    }

    fn keyword_search(&self) -> Result<&'static str, EngineError> {
        Ok("SELECT rowid FROM bio_fts WHERE bio_fts MATCH $term ORDER BY rank LIMIT 10")
    }

    fn vector_search(&self) -> Result<&'static str, EngineError> {
        Err(EngineError::Unsupported(
            "SQLite has no vector distance function".to_owned(),
        ))
    }
}

fn to_sql(param: &Param) -> Sql {
    match param {
        Param::Int(value) => Sql::Integer(*value),
        Param::Text(value) => Sql::Text(value.clone()),
        Param::Floats(values) => Sql::Blob(blob(values)),
        Param::Ints(values) => Sql::Text(json_list(values)),
        Param::NullInt => Sql::Null,
        Param::Rows(_) => Sql::Null,
    }
}

fn json_list(values: &[i64]) -> String {
    format!(
        "[{}]",
        values
            .iter()
            .map(i64::to_string)
            .collect::<Vec<_>>()
            .join(",")
    )
}

fn bind(statement: &mut Statement<'_>, params: &[(&str, Param)]) -> Result<(), EngineError> {
    for (name, param) in params {
        if let Some(index) = statement
            .parameter_index(&format!("${name}"))
            .map_err(failed)?
        {
            statement
                .raw_bind_parameter(index, to_sql(param))
                .map_err(failed)?;
        }
    }
    Ok(())
}

impl Session for SqliteSession {
    fn prepare(&mut self, text: &str) -> Result<(), EngineError> {
        if !text.contains(';') {
            self.connection.prepare_cached(text).map_err(failed)?;
        }
        Ok(())
    }

    fn run(&mut self, query: &Query<'_>) -> Result<Fingerprint, EngineError> {
        self.walk(query, None)
    }

    fn ids(&mut self, query: &Query<'_>) -> Result<Vec<i64>, EngineError> {
        let mut ids = Vec::new();
        self.walk(query, Some(&mut ids))?;
        Ok(ids)
    }

    fn begin(&mut self) -> Result<(), EngineError> {
        self.connection.execute_batch("BEGIN").map_err(failed)
    }

    fn commit(&mut self) -> Result<(), EngineError> {
        self.connection.execute_batch("COMMIT").map_err(failed)
    }
}

impl SqliteSession {
    fn walk(
        &mut self,
        query: &Query<'_>,
        mut ids: Option<&mut Vec<i64>>,
    ) -> Result<Fingerprint, EngineError> {
        if query.text.contains(';') {
            self.connection.execute_batch(query.text).map_err(failed)?;
            return Ok(Fingerprint::default());
        }
        let mut statement = self.connection.prepare_cached(query.text).map_err(failed)?;
        let rows = query.params.iter().find_map(|(_, param)| match param {
            Param::Rows(rows) => Some(rows),
            _ => None,
        });
        if let Some(rows) = rows {
            for row in rows {
                bind(&mut statement, row)?;
                statement.raw_execute().map_err(failed)?;
            }
            return Ok(Fingerprint::default());
        }
        bind(&mut statement, &query.params)?;
        let columns = statement.column_count();
        if columns == 0 {
            statement.raw_execute().map_err(failed)?;
            return Ok(Fingerprint::default());
        }
        let mut fingerprint = Fingerprint::default();
        let mut result = statement.raw_query();
        while let Some(row) = result.next().map_err(failed)? {
            let mut hasher = RowHasher::default();
            for column in 0..columns {
                let value = row.get_ref(column).map_err(failed)?;
                if let (0, Some(ids), ValueRef::Integer(id)) = (column, ids.as_deref_mut(), value) {
                    ids.push(id);
                }
                match value {
                    ValueRef::Null => hasher.null(),
                    ValueRef::Integer(value) => hasher.int(value),
                    ValueRef::Real(value) => hasher.float(value),
                    ValueRef::Text(bytes) => hasher.text(&String::from_utf8_lossy(bytes)),
                    ValueRef::Blob(bytes) => hasher.text(&format!("{} bytes", bytes.len())),
                }
            }
            fingerprint.absorb(hasher);
        }
        Ok(fingerprint)
    }
}

#[cfg(test)]
#[path = "sqlite.test.rs"]
mod tests;
