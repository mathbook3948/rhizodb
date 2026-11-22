use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;

use cozo::DataValue;
use cozo::DbInstance;
use cozo::MultiTransaction;
use cozo::NamedRows;
use cozo::Num;
use cozo::ScriptMutability;

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

const BATCH: usize = 20_000;
const DIRECTORY: &str = "cozo.rocksdb";

const SCHEMA: [&str; 5] = [
    ":create persons {id: Int => name: String, age: Int?, bio: String, embedding: <F32; 64>}",
    ":create cities {id: Int => name: String}",
    ":create knows {src: Int, dst: Int => since: Int}",
    ":create lives_in {src: Int => dst: Int}",
    "::index create knows:by_dst {dst, src}",
];

const PUT_PERSONS: &str =
    "?[id, name, age, bio, embedding] <- $rows :put persons {id => name, age, bio, embedding}";
const PUT_CITIES: &str = "?[id, name] <- $rows :put cities {id => name}";
const PUT_KNOWS: &str = "?[src, dst, since] <- $rows :put knows {src, dst => since}";
const PUT_LIVES_IN: &str = "?[src, dst] <- $rows :put lives_in {src => dst}";

/// CozoDB through the `cozo` crate with RocksDB storage in a directory, and Datalog queries.
///
/// The SQLite storage cannot be used, because `rusqlite` and CozoDB's SQLite binding both claim
/// the native library `sqlite3` and cargo allows one such crate per build.
#[derive(Clone, Copy, Debug)]
pub struct CozoEngine;

type Batches = Vec<(&'static str, BTreeMap<String, DataValue>)>;

struct CozoInstance {
    db: DbInstance,
    prepared: Mutex<Option<Batches>>,
}

impl std::fmt::Debug for CozoInstance {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CozoInstance")
            .finish_non_exhaustive()
    }
}

struct CozoSession<'a> {
    db: &'a DbInstance,
    transaction: Option<MultiTransaction>,
}

fn failed(error: impl std::fmt::Display) -> EngineError {
    EngineError::Failed(error.to_string())
}

impl Engine for CozoEngine {
    fn kind(&self) -> EngineKind {
        EngineKind::Cozodb
    }

    fn dialect(&self) -> Dialect {
        Dialect::Datalog
    }

    fn open(&self, dir: &Path) -> Result<Box<dyn Instance>, EngineError> {
        let db = DbInstance::new("rocksdb", dir.join(DIRECTORY), "").map_err(failed)?;
        Ok(Box::new(CozoInstance {
            db,
            prepared: Mutex::new(None),
        }))
    }

    fn config(&self) -> serde_json::Value {
        serde_json::json!({
            "version": env!("PIN_COZO"),
            "pinned": env!("PIN_COZO"),
            "storage": "file",
            "backend": "RocksDB, library defaults",
            "threads": "library default",
            "indexes": "knows by dst, keys on every id and on knows(src, dst)",
            "keyword_search": "FTS, Simple tokenizer with Lowercase filter",
            "vector_search": "HNSW, cosine, m 16, ef_construction 100, ef 64",
        })
    }
}

fn int(value: i64) -> DataValue {
    DataValue::from(value)
}

fn float_list(values: &[f64]) -> DataValue {
    DataValue::List(values.iter().map(|&value| DataValue::from(value)).collect())
}

fn batch(
    statement: &'static str,
    rows: Vec<DataValue>,
) -> (&'static str, BTreeMap<String, DataValue>) {
    (
        statement,
        BTreeMap::from([("rows".to_owned(), DataValue::List(rows))]),
    )
}

impl Instance for CozoInstance {
    fn session(&self) -> Result<Box<dyn Session + '_>, EngineError> {
        Ok(Box::new(CozoSession {
            db: &self.db,
            transaction: None,
        }))
    }

    fn create_schema(&self) -> Result<(), EngineError> {
        for statement in SCHEMA {
            self.db.run_default(statement).map_err(failed)?;
        }
        Ok(())
    }

    fn prepare_bulk_load(&self, data: &Dataset, _scratch: &Path) -> Result<(), EngineError> {
        let mut batches: Batches = Vec::new();
        for people in data.people.chunks(BATCH) {
            let rows = people
                .iter()
                .map(|person| {
                    DataValue::List(vec![
                        int(person.id),
                        DataValue::from(person.name.as_str()),
                        person.age.map_or(DataValue::Null, int),
                        DataValue::from(person.bio.as_str()),
                        float_list(&person.embedding),
                    ])
                })
                .collect();
            batches.push(batch(PUT_PERSONS, rows));
        }
        let cities = data
            .cities
            .iter()
            .map(|city| DataValue::List(vec![int(city.id), DataValue::from(city.name.as_str())]))
            .collect();
        batches.push(batch(PUT_CITIES, cities));
        for edges in data.knows.chunks(BATCH) {
            let rows = edges
                .iter()
                .map(|edge| DataValue::List(vec![int(edge.from), int(edge.to), int(edge.since)]))
                .collect();
            batches.push(batch(PUT_KNOWS, rows));
        }
        let lives_in = data
            .lives_in
            .iter()
            .map(|edge| DataValue::List(vec![int(edge.person), int(edge.city)]))
            .collect();
        batches.push(batch(PUT_LIVES_IN, lives_in));
        *self.prepared.lock().map_err(|_| poisoned())? = Some(batches);
        Ok(())
    }

    fn bulk_load(&self, _data: &Dataset, _scratch: &Path) -> Result<(), EngineError> {
        let batches = self
            .prepared
            .lock()
            .map_err(|_| poisoned())?
            .take()
            .ok_or_else(|| EngineError::Failed("prepare_bulk_load was not called".to_owned()))?;
        let transaction = self.db.multi_transaction(true);
        for (statement, params) in batches {
            transaction.run_script(statement, params).map_err(failed)?;
        }
        transaction.commit().map_err(failed)
    }

    fn create_keyword_index(&self) -> Result<Built, EngineError> {
        self.db
            .run_default(
                "::fts create persons:bio_fts {extractor: bio, tokenizer: Simple, filters: [Lowercase]}",
            )
            .map_err(failed)?;
        Ok(Built::Index)
    }

    fn create_vector_index(&self) -> Result<Built, EngineError> {
        self.db
            .run_default(
                "::hnsw create persons:vec {dim: 64, m: 16, dtype: F32, fields: [embedding], distance: Cosine, ef_construction: 100}",
            )
            .map_err(failed)?;
        Ok(Built::Index)
    }

    fn keyword_search(&self) -> Result<&'static str, EngineError> {
        Ok(
            "?[id, score] := ~persons:bio_fts{id | query: $term, k: 10, bind_score: score} :order -score",
        )
    }

    fn vector_search(&self) -> Result<&'static str, EngineError> {
        Ok(
            "?[id, distance] := ~persons:vec{id | query: vec($vector), k: 10, ef: 64, bind_distance: distance} :order distance",
        )
    }
}

fn poisoned() -> EngineError {
    EngineError::Failed("the prepared input lock is poisoned".to_owned())
}

fn value(param: &Param) -> DataValue {
    match param {
        Param::Int(value) => int(*value),
        Param::Text(value) => DataValue::from(value.as_str()),
        Param::Floats(values) => float_list(values),
        Param::NullInt => DataValue::Null,
        Param::Rows(rows) => DataValue::List(
            rows.iter()
                .map(|row| DataValue::List(row.iter().map(|(_, param)| value(param)).collect()))
                .collect(),
        ),
    }
}

fn parameters(query: &Query<'_>) -> BTreeMap<String, DataValue> {
    query
        .params
        .iter()
        .map(|(name, param)| ((*name).to_owned(), value(param)))
        .collect()
}

fn mutates(text: &str) -> bool {
    [":put", ":rm", ":update", ":create"]
        .iter()
        .any(|command| text.contains(command))
}

impl Session for CozoSession<'_> {
    fn prepare(&mut self, _text: &str) -> Result<(), EngineError> {
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
        self.transaction = Some(self.db.multi_transaction(true));
        Ok(())
    }

    fn commit(&mut self) -> Result<(), EngineError> {
        match self.transaction.take() {
            Some(transaction) => transaction.commit().map_err(failed),
            None => Ok(()),
        }
    }
}

impl CozoSession<'_> {
    fn walk(
        &mut self,
        query: &Query<'_>,
        mut ids: Option<&mut Vec<i64>>,
    ) -> Result<Fingerprint, EngineError> {
        let parameters = parameters(query);
        let writes = mutates(query.text);
        let result: NamedRows = match self.transaction.as_ref() {
            Some(transaction) => transaction
                .run_script(query.text, parameters)
                .map_err(failed)?,
            None => {
                let mutability = if writes {
                    ScriptMutability::Mutable
                } else {
                    ScriptMutability::Immutable
                };
                self.db
                    .run_script(query.text, parameters, mutability)
                    .map_err(failed)?
            }
        };
        let mut fingerprint = Fingerprint::default();
        if writes {
            return Ok(fingerprint);
        }
        for row in &result.rows {
            if let (Some(ids), Some(DataValue::Num(Num::Int(id)))) =
                (ids.as_deref_mut(), row.first())
            {
                ids.push(*id);
            }
            let mut hasher = RowHasher::default();
            row.iter().for_each(|value| hash(&mut hasher, value));
            fingerprint.absorb(hasher);
        }
        Ok(fingerprint)
    }
}

fn hash(hasher: &mut RowHasher, value: &DataValue) {
    match value {
        DataValue::Null => hasher.null(),
        DataValue::Bool(value) => hasher.boolean(*value),
        DataValue::Num(Num::Int(value)) => hasher.int(*value),
        DataValue::Num(Num::Float(value)) => hasher.float(*value),
        DataValue::Str(value) => hasher.text(value),
        DataValue::List(items) => {
            hasher.open_list();
            items.iter().for_each(|item| hash(hasher, item));
            hasher.close_list();
        }
        other => hasher.text(&format!("{other:?}")),
    }
}

#[cfg(test)]
#[path = "cozo.test.rs"]
mod tests;
