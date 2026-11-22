use std::collections::HashMap;
use std::path::Path;

use lbug::Connection;
use lbug::Database;
use lbug::LogicalType;
use lbug::PreparedStatement;
use lbug::SystemConfig;
use lbug::Value;

use crate::data::CITIES_CSV;
use crate::data::Dataset;
use crate::data::KNOWS_CSV;
use crate::data::LIVES_IN_CSV;
use crate::data::PERSONS_CSV;
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

const FILE_NAME: &str = "ladybug.lbdb";

const SCHEMA: [&str; 4] = [
    "CREATE NODE TABLE Person(id INT64 PRIMARY KEY, name STRING, age INT64, bio STRING, embedding FLOAT[64])",
    "CREATE NODE TABLE City(id INT64 PRIMARY KEY, name STRING)",
    "CREATE REL TABLE KNOWS(FROM Person TO Person, since INT64)",
    "CREATE REL TABLE LIVES_IN(FROM Person TO City)",
];

/// LadybugDB, opened as a file with its default configuration.
#[derive(Clone, Copy, Debug)]
pub struct LadybugEngine;

struct LadybugInstance {
    database: Database,
}

struct LadybugSession<'a> {
    connection: Connection<'a>,
    prepared: HashMap<String, PreparedStatement>,
}

impl std::fmt::Debug for LadybugInstance {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LadybugInstance")
            .finish_non_exhaustive()
    }
}

impl Engine for LadybugEngine {
    fn kind(&self) -> EngineKind {
        EngineKind::Ladybugdb
    }

    fn dialect(&self) -> Dialect {
        Dialect::Cypher
    }

    fn open(&self, dir: &Path) -> Result<Box<dyn Instance>, EngineError> {
        let database =
            Database::new(dir.join(FILE_NAME), SystemConfig::default()).map_err(failed)?;
        Ok(Box::new(LadybugInstance { database }))
    }
}

impl LadybugInstance {
    fn execute(&self, statement: &str) -> Result<(), EngineError> {
        let connection = Connection::new(&self.database).map_err(failed)?;
        connection.query(statement).map_err(failed)?;
        Ok(())
    }

    fn extension(&self, name: &str) -> Result<(), EngineError> {
        self.execute(&format!("INSTALL {name}"))
            .map_err(unsupported)?;
        self.execute(&format!("LOAD EXTENSION {name}"))
            .map_err(unsupported)
    }
}

impl Instance for LadybugInstance {
    fn session(&self) -> Result<Box<dyn Session + '_>, EngineError> {
        Ok(Box::new(LadybugSession {
            connection: Connection::new(&self.database).map_err(failed)?,
            prepared: HashMap::new(),
        }))
    }

    fn create_schema(&self) -> Result<(), EngineError> {
        SCHEMA
            .iter()
            .try_for_each(|statement| self.execute(statement))
    }

    fn prepare_bulk_load(&self, data: &Dataset, scratch: &Path) -> Result<(), EngineError> {
        data.write_csv(scratch)
            .map_err(|error| EngineError::Failed(error.to_string()))
    }

    fn bulk_load(&self, _data: &Dataset, scratch: &Path) -> Result<(), EngineError> {
        for (table, file) in [
            ("Person", PERSONS_CSV),
            ("City", CITIES_CSV),
            ("KNOWS", KNOWS_CSV),
            ("LIVES_IN", LIVES_IN_CSV),
        ] {
            let path = scratch.join(file);
            self.execute(&format!(
                "COPY {table} FROM '{}' (HEADER=true)",
                path.display()
            ))?;
        }
        Ok(())
    }

    fn create_keyword_index(&self) -> Result<(), EngineError> {
        self.extension("FTS")?;
        self.execute("CALL CREATE_FTS_INDEX('Person', 'bio_index', ['bio'])")
    }

    fn create_vector_index(&self) -> Result<(), EngineError> {
        self.extension("VECTOR")?;
        self.execute(
            "CALL CREATE_VECTOR_INDEX('Person', 'embedding_index', 'embedding', metric := 'cosine')",
        )
    }

    fn keyword_search(&self) -> &'static str {
        "CALL QUERY_FTS_INDEX('Person', 'bio_index', $term, top := 10) RETURN node.id AS id, score ORDER BY score DESC LIMIT 10"
    }

    fn vector_search(&self) -> &'static str {
        "CALL QUERY_VECTOR_INDEX('Person', 'embedding_index', $vector, 10) RETURN node.id AS id, distance ORDER BY distance LIMIT 10"
    }
}

impl Session for LadybugSession<'_> {
    fn prepare(&mut self, text: &str) -> Result<(), EngineError> {
        if !self.prepared.contains_key(text) {
            let statement = self.connection.prepare(text).map_err(failed)?;
            self.prepared.insert(text.to_owned(), statement);
        }
        Ok(())
    }

    fn run(&mut self, query: &Query<'_>) -> Result<Fingerprint, EngineError> {
        self.prepare(query.text)?;
        let statement = self
            .prepared
            .get_mut(query.text)
            .ok_or_else(|| EngineError::Failed("prepared statement is missing".to_owned()))?;
        let params: Vec<(&str, Value)> = query
            .params
            .iter()
            .map(|(name, param)| (*name, value(param)))
            .collect();
        let result = self.connection.execute(statement, params).map_err(failed)?;
        let mut fingerprint = Fingerprint::default();
        for row in result {
            let mut hasher = RowHasher::default();
            row.iter().for_each(|value| hash(&mut hasher, value));
            fingerprint.absorb(hasher);
        }
        Ok(fingerprint)
    }

    fn begin(&mut self) -> Result<(), EngineError> {
        self.connection
            .query("BEGIN TRANSACTION")
            .map_err(failed)
            .map(drop)
    }

    fn commit(&mut self) -> Result<(), EngineError> {
        self.connection.query("COMMIT").map_err(failed).map(drop)
    }
}

fn failed(error: lbug::Error) -> EngineError {
    EngineError::Failed(error.to_string())
}

fn unsupported(error: EngineError) -> EngineError {
    match error {
        EngineError::Failed(message) => EngineError::Unsupported(message),
        other => other,
    }
}

fn value(param: &Param) -> Value {
    match param {
        Param::Int(value) => Value::Int64(*value),
        Param::Text(value) => Value::String(value.clone()),
        Param::Floats(values) => Value::List(
            LogicalType::Float,
            values
                .iter()
                .map(|&value| Value::Float(value as f32))
                .collect(),
        ),
        Param::NullInt => Value::Null(LogicalType::Int64),
        Param::Rows(rows) => Value::List(
            LogicalType::Struct {
                fields: rows.first().map_or_else(Vec::new, |row| {
                    row.iter()
                        .map(|(name, param)| ((*name).to_owned(), logical_type(param)))
                        .collect()
                }),
            },
            rows.iter()
                .map(|row| {
                    Value::Struct(
                        row.iter()
                            .map(|(name, param)| ((*name).to_owned(), value(param)))
                            .collect(),
                    )
                })
                .collect(),
        ),
    }
}

fn logical_type(param: &Param) -> LogicalType {
    match param {
        Param::Int(_) | Param::NullInt => LogicalType::Int64,
        Param::Text(_) => LogicalType::String,
        Param::Floats(_) => LogicalType::List {
            child_type: Box::new(LogicalType::Float),
        },
        Param::Rows(_) => LogicalType::Any,
    }
}

fn hash(hasher: &mut RowHasher, value: &Value) {
    match value {
        Value::Null(_) => hasher.null(),
        Value::Bool(value) => hasher.boolean(*value),
        Value::Int64(value) => hasher.int(*value),
        Value::Int32(value) => hasher.int(i64::from(*value)),
        Value::Int16(value) => hasher.int(i64::from(*value)),
        Value::Int8(value) => hasher.int(i64::from(*value)),
        Value::UInt8(value) => hasher.int(i64::from(*value)),
        Value::UInt16(value) => hasher.int(i64::from(*value)),
        Value::UInt32(value) => hasher.int(i64::from(*value)),
        Value::UInt64(value) => match i64::try_from(*value) {
            Ok(value) => hasher.int(value),
            Err(_) => hasher.text(&value.to_string()),
        },
        Value::Int128(value) => match i64::try_from(*value) {
            Ok(value) => hasher.int(value),
            Err(_) => hasher.text(&value.to_string()),
        },
        Value::Double(value) => hasher.float(*value),
        Value::Float(value) => hasher.float(f64::from(*value)),
        Value::String(value) => hasher.text(value),
        Value::List(_, items) | Value::Array(_, items) => {
            hasher.open_list();
            items.iter().for_each(|item| hash(hasher, item));
            hasher.close_list();
        }
        other => hasher.text(&other.to_string()),
    }
}

#[cfg(test)]
#[path = "ladybug.test.rs"]
mod tests;
