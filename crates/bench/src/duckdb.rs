use std::path::Path;
use std::sync::Mutex;

use duckdb::Connection;
use duckdb::ToSql;
use duckdb::types::Value as Sql;
use duckdb::types::ValueRef;

use crate::data::CITIES_CSV;
use crate::data::Dataset;
use crate::data::KNOWS_CSV;
use crate::data::LIVES_IN_CSV;
use crate::data::PERSONS_CSV;
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

const FILE_NAME: &str = "duckdb.db";

const SCHEMA: &str = "
CREATE TABLE persons(id BIGINT PRIMARY KEY, name VARCHAR, age BIGINT, bio VARCHAR, embedding FLOAT[64]);
CREATE TABLE cities(id BIGINT PRIMARY KEY, name VARCHAR);
CREATE TABLE knows(src BIGINT NOT NULL, dst BIGINT NOT NULL, since BIGINT);
CREATE TABLE lives_in(src BIGINT PRIMARY KEY, dst BIGINT NOT NULL);
CREATE INDEX knows_by_src ON knows(src);
CREATE INDEX knows_by_dst ON knows(dst);
";
const DROP_INDEXES: &str = "DROP INDEX knows_by_src; DROP INDEX knows_by_dst;";
const CREATE_INDEXES: &str = "
CREATE INDEX knows_by_src ON knows(src);
CREATE INDEX knows_by_dst ON knows(dst);
";

/// DuckDB through the `duckdb` crate, bundled and built from source, with one database file.
/// Bulk loading reads the CSV export, as DuckDB's own loader does, and a close checkpoints.
#[derive(Clone, Copy, Debug)]
pub struct DuckdbEngine;

struct DuckdbInstance {
    primary: Mutex<Option<Connection>>,
}

impl std::fmt::Debug for DuckdbInstance {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DuckdbInstance")
            .finish_non_exhaustive()
    }
}

struct DuckdbSession {
    connection: Connection,
}

fn failed(error: duckdb::Error) -> EngineError {
    EngineError::Failed(error.to_string())
}

impl Engine for DuckdbEngine {
    fn kind(&self) -> EngineKind {
        EngineKind::Duckdb
    }

    fn dialect(&self) -> Dialect {
        Dialect::Sql
    }

    fn open(&self, dir: &Path) -> Result<Box<dyn Instance>, EngineError> {
        let path = dir.join(FILE_NAME);
        let connection = Connection::open(&path).map_err(failed)?;
        Ok(Box::new(DuckdbInstance {
            primary: Mutex::new(Some(connection)),
        }))
    }

    fn config(&self) -> serde_json::Value {
        let probe = Connection::open_in_memory().ok().map(|connection| {
            let one = |setting: &str| {
                connection
                    .query_row(
                        &format!("SELECT CAST(current_setting('{setting}') AS VARCHAR)"),
                        [],
                        |row| row.get::<_, String>(0),
                    )
                    .ok()
            };
            let version = connection
                .query_row("SELECT version()", [], |row| row.get::<_, String>(0))
                .ok();
            (version, one("threads"), one("memory_limit"))
        });
        let (version, threads, memory) = probe.unwrap_or((None, None, None));
        serde_json::json!({
            "version": version,
            "pinned": env!("PIN_DUCKDB"),
            "storage": "file",
            "threads": threads,
            "memory_limit": memory,
            "indexes": "ART on knows(src), knows(dst), primary keys on every id",
            "keyword_search": "scan with list_contains over the split bio, no index",
            "vector_search": "scan with array_cosine_distance over FLOAT[64], no index",
        })
    }
}

impl DuckdbInstance {
    fn with<T>(
        &self,
        work: impl FnOnce(&Connection) -> Result<T, duckdb::Error>,
    ) -> Result<T, EngineError> {
        let guard = self.primary.lock().map_err(|_| {
            EngineError::Failed("the primary connection lock is poisoned".to_owned())
        })?;
        let connection = guard
            .as_ref()
            .ok_or_else(|| EngineError::Failed("the database is closed".to_owned()))?;
        work(connection).map_err(failed)
    }
}

impl Instance for DuckdbInstance {
    fn session(&self) -> Result<Box<dyn Session + '_>, EngineError> {
        let connection = self.with(Connection::try_clone)?;
        Ok(Box::new(DuckdbSession { connection }))
    }

    fn create_schema(&self) -> Result<(), EngineError> {
        self.with(|connection| connection.execute_batch(SCHEMA))
    }

    fn prepare_bulk_load(&self, data: &Dataset, scratch: &Path) -> Result<(), EngineError> {
        data.write_csv(scratch)
            .map_err(|error| EngineError::Failed(error.to_string()))
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

    fn bulk_load(&self, _data: &Dataset, scratch: &Path) -> Result<(), EngineError> {
        let file = |name: &str| scratch.join(name).display().to_string();
        let script = format!(
            "BEGIN;
             {DROP_INDEXES}
             INSERT INTO persons SELECT id, name, age, bio, CAST(embedding AS FLOAT[64])
               FROM read_csv('{persons}', header = true,
                 columns = {{'id': 'BIGINT', 'name': 'VARCHAR', 'age': 'BIGINT', 'bio': 'VARCHAR', 'embedding': 'VARCHAR'}});
             INSERT INTO cities SELECT * FROM read_csv('{cities}', header = true,
                 columns = {{'id': 'BIGINT', 'name': 'VARCHAR'}});
             INSERT INTO knows SELECT * FROM read_csv('{knows}', header = true,
                 columns = {{'src': 'BIGINT', 'dst': 'BIGINT', 'since': 'BIGINT'}});
             INSERT INTO lives_in SELECT * FROM read_csv('{lives_in}', header = true,
                 columns = {{'src': 'BIGINT', 'dst': 'BIGINT'}});
             {CREATE_INDEXES}
             COMMIT;",
            persons = file(PERSONS_CSV),
            cities = file(CITIES_CSV),
            knows = file(KNOWS_CSV),
            lives_in = file(LIVES_IN_CSV),
        );
        self.with(|connection| connection.execute_batch(&script))
    }

    fn create_keyword_index(&self) -> Result<Built, EngineError> {
        Ok(Built::Scan)
    }

    fn create_vector_index(&self) -> Result<Built, EngineError> {
        Ok(Built::Scan)
    }

    fn keyword_search(&self) -> Result<&'static str, EngineError> {
        Ok(
            "SELECT id FROM persons WHERE list_contains(string_split(bio, ' '), $term) ORDER BY id LIMIT 10",
        )
    }

    fn vector_search(&self) -> Result<&'static str, EngineError> {
        Ok(
            "SELECT id FROM persons ORDER BY array_cosine_distance(embedding, CAST($vector AS FLOAT[64])), id LIMIT 10",
        )
    }
}

fn to_sql(param: &Param) -> Sql {
    match param {
        Param::Int(value) => Sql::BigInt(*value),
        Param::Text(value) => Sql::Text(value.clone()),
        Param::Floats(values) => Sql::Text(format!(
            "[{}]",
            values
                .iter()
                .map(f64::to_string)
                .collect::<Vec<_>>()
                .join(",")
        )),
        Param::NullInt => Sql::Null,
        Param::Rows(_) => Sql::Null,
    }
}

fn placeholders(text: &str) -> Vec<&str> {
    let mut names: Vec<&str> = Vec::new();
    let bytes = text.as_bytes();
    let mut start = 0;
    while let Some(offset) = text[start..].find('$') {
        let begin = start + offset + 1;
        let end = bytes[begin..]
            .iter()
            .position(|byte| !(byte.is_ascii_alphanumeric() || *byte == b'_'))
            .map_or(bytes.len(), |length| begin + length);
        let name = &text[begin..end];
        if !name.is_empty() && !names.contains(&name) {
            names.push(name);
        }
        start = end.max(begin);
    }
    names
}

fn named<'a>(text: &'a str, params: &'a [(&'static str, Param)]) -> Vec<(&'a str, Sql)> {
    placeholders(text)
        .into_iter()
        .filter_map(|name| {
            params
                .iter()
                .find(|(given, _)| *given == name)
                .map(|(_, param)| (name, to_sql(param)))
        })
        .collect()
}

fn borrowed<'a>(values: &'a [(&'a str, Sql)]) -> Vec<(&'a str, &'a dyn ToSql)> {
    values
        .iter()
        .map(|(name, value)| (*name, value as &dyn ToSql))
        .collect()
}

fn returns_rows(text: &str) -> bool {
    let upper = text.trim_start().to_ascii_uppercase();
    upper.starts_with("SELECT") || upper.starts_with("WITH")
}

impl Session for DuckdbSession {
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

impl DuckdbSession {
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
                let values = named(query.text, row);
                statement.execute(&borrowed(&values)[..]).map_err(failed)?;
            }
            return Ok(Fingerprint::default());
        }
        let values = named(query.text, &query.params);
        if !returns_rows(query.text) {
            statement.execute(&borrowed(&values)[..]).map_err(failed)?;
            return Ok(Fingerprint::default());
        }
        let mut fingerprint = Fingerprint::default();
        let mut result = statement.query(&borrowed(&values)[..]).map_err(failed)?;
        while let Some(row) = result.next().map_err(failed)? {
            let mut hasher = RowHasher::default();
            let mut column = 0;
            while let Ok(value) = row.get_ref(column) {
                if let (0, Some(ids), Some(id)) = (column, ids.as_deref_mut(), integer(&value)) {
                    ids.push(id);
                }
                hash(&mut hasher, &value);
                column += 1;
            }
            fingerprint.absorb(hasher);
        }
        Ok(fingerprint)
    }
}

fn integer(value: &ValueRef<'_>) -> Option<i64> {
    match value {
        ValueRef::TinyInt(value) => Some(i64::from(*value)),
        ValueRef::SmallInt(value) => Some(i64::from(*value)),
        ValueRef::Int(value) => Some(i64::from(*value)),
        ValueRef::BigInt(value) => Some(*value),
        ValueRef::HugeInt(value) => i64::try_from(*value).ok(),
        _ => None,
    }
}

fn hash(hasher: &mut RowHasher, value: &ValueRef<'_>) {
    match value {
        ValueRef::Null => hasher.null(),
        ValueRef::Boolean(value) => hasher.boolean(*value),
        ValueRef::UTinyInt(value) => hasher.int(i64::from(*value)),
        ValueRef::USmallInt(value) => hasher.int(i64::from(*value)),
        ValueRef::UInt(value) => hasher.int(i64::from(*value)),
        ValueRef::UBigInt(value) => match i64::try_from(*value) {
            Ok(value) => hasher.int(value),
            Err(_) => hasher.text(&value.to_string()),
        },
        ValueRef::Float(value) => hasher.float(f64::from(*value)),
        ValueRef::Double(value) => hasher.float(*value),
        ValueRef::Text(bytes) => hasher.text(&String::from_utf8_lossy(bytes)),
        other => match integer(other) {
            Some(value) => hasher.int(value),
            None => hasher.text(&format!("{other:?}")),
        },
    }
}

#[cfg(test)]
#[path = "duckdb.test.rs"]
mod tests;
