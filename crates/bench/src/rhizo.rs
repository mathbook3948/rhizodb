use std::path::Path;
use std::sync::Mutex;

use rhizodb::Database;
use rhizodb::IndexTarget;
use rhizodb::Parameters;
use rhizodb::Value;
use rhizodb::VectorMetric;
use rhizodb::WriteTransaction;

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

const NODE_BATCH: usize = 5_000;
const EDGE_BATCH: usize = 20_000;
/// The name of the database file RhizoDB stores the benchmark graph in.
pub const FILE_NAME: &str = "graph.rhizodb";

const CREATE_PERSONS: &str = "UNWIND $rows AS r CREATE (p:Person {id: r.id, name: r.name, age: r.age, bio: r.bio, embedding: r.embedding}) RETURN id(p)";
const CREATE_CITIES: &str =
    "UNWIND $rows AS r CREATE (c:City {id: r.id, name: r.name}) RETURN id(c)";
const CREATE_KNOWS: &str = "UNWIND range(0, size($from) - 1) AS i MATCH (a) WHERE id(a) = $from[i] MATCH (b) WHERE id(b) = $to[i] CREATE (a)-[:KNOWS {since: $since[i]}]->(b)";
const CREATE_LIVES_IN: &str = "UNWIND range(0, size($from) - 1) AS i MATCH (a) WHERE id(a) = $from[i] MATCH (b) WHERE id(b) = $to[i] CREATE (a)-[:LIVES_IN]->(b)";

/// RhizoDB, opened as a file.
#[derive(Clone, Copy, Debug)]
pub struct RhizoEngine;

#[derive(Debug)]
struct Prepared {
    persons: Vec<Parameters>,
    cities: Vec<Parameters>,
    knows: Vec<EdgeBatch>,
    lives_in: Vec<EdgeBatch>,
}

#[derive(Debug)]
struct EdgeBatch {
    from: Vec<usize>,
    to: Vec<usize>,
    since: Option<Value>,
}

#[derive(Debug)]
struct RhizoInstance {
    database: Database,
    prepared: Mutex<Option<Prepared>>,
}

struct RhizoSession<'a> {
    database: &'a Database,
    transaction: Option<WriteTransaction<'a>>,
}

impl Engine for RhizoEngine {
    fn kind(&self) -> EngineKind {
        EngineKind::Rhizodb
    }

    fn dialect(&self) -> Dialect {
        Dialect::Gql
    }

    fn open(&self, dir: &Path) -> Result<Box<dyn Instance>, EngineError> {
        let database = Database::open(dir.join(FILE_NAME)).map_err(failed)?;
        Ok(Box::new(RhizoInstance {
            database,
            prepared: Mutex::new(None),
        }))
    }

    fn config(&self) -> serde_json::Value {
        serde_json::json!({
            "version": env!("PIN_RHIZODB"),
            "pinned": "path dependency on crates/rhizodb",
            "storage": "file",
            "threads": "readers in parallel, one writer",
            "buffer_pool": "library default",
        })
    }
}

impl Instance for RhizoInstance {
    fn session(&self) -> Result<Box<dyn Session + '_>, EngineError> {
        Ok(Box::new(RhizoSession {
            database: &self.database,
            transaction: None,
        }))
    }

    fn create_schema(&self) -> Result<(), EngineError> {
        self.database
            .create_property_index("person_id", IndexTarget::Label("Person"), "id")
            .map_err(failed)?;
        self.database
            .create_property_index("city_id", IndexTarget::Label("City"), "id")
            .map_err(failed)
    }

    fn prepare_bulk_load(&self, data: &Dataset, _scratch: &Path) -> Result<(), EngineError> {
        let index = |id: i64| usize::try_from(id - 1).unwrap_or(usize::MAX);
        let prepared = Prepared {
            persons: person_rows(data)
                .chunks(NODE_BATCH)
                .map(|batch| rows_parameter(batch.to_vec()))
                .collect(),
            cities: city_rows(data)
                .chunks(NODE_BATCH)
                .map(|batch| rows_parameter(batch.to_vec()))
                .collect(),
            knows: data
                .knows
                .chunks(EDGE_BATCH)
                .map(|batch| EdgeBatch {
                    from: batch.iter().map(|edge| index(edge.from)).collect(),
                    to: batch.iter().map(|edge| index(edge.to)).collect(),
                    since: Some(Value::List(
                        batch
                            .iter()
                            .map(|edge| Value::Integer(edge.since))
                            .collect(),
                    )),
                })
                .collect(),
            lives_in: data
                .lives_in
                .chunks(EDGE_BATCH)
                .map(|batch| EdgeBatch {
                    from: batch.iter().map(|edge| index(edge.person)).collect(),
                    to: batch.iter().map(|edge| index(edge.city)).collect(),
                    since: None,
                })
                .collect(),
        };
        *self.prepared.lock().map_err(|_| poisoned())? = Some(prepared);
        Ok(())
    }

    fn bulk_load(&self, _data: &Dataset, _scratch: &Path) -> Result<(), EngineError> {
        let prepared = self
            .prepared
            .lock()
            .map_err(|_| poisoned())?
            .take()
            .ok_or_else(|| EngineError::Failed("prepare_bulk_load was not called".to_owned()))?;
        let mut transaction = self.database.begin_write().map_err(failed)?;
        let persons = create_nodes(&mut transaction, CREATE_PERSONS, &prepared.persons)?;
        let cities = create_nodes(&mut transaction, CREATE_CITIES, &prepared.cities)?;
        create_edges(
            &mut transaction,
            CREATE_KNOWS,
            prepared.knows,
            &persons,
            &persons,
        )?;
        create_edges(
            &mut transaction,
            CREATE_LIVES_IN,
            prepared.lives_in,
            &persons,
            &cities,
        )?;
        transaction.commit().map_err(failed)
    }

    fn create_keyword_index(&self) -> Result<Built, EngineError> {
        self.database
            .create_keyword_index("bio", IndexTarget::Label("Person"), &["bio"])
            .map_err(failed)?;
        Ok(Built::Index)
    }

    fn create_vector_index(&self) -> Result<Built, EngineError> {
        self.database
            .create_vector_index(
                "embedding",
                IndexTarget::Label("Person"),
                "embedding",
                64,
                VectorMetric::Cosine,
            )
            .map_err(failed)?;
        Ok(Built::Index)
    }

    fn keyword_search(&self) -> Result<&'static str, EngineError> {
        Ok(
            "CALL db.index.keyword.queryNodes('bio', $term, 10) YIELD node, score RETURN node.id AS id",
        )
    }

    fn vector_search(&self) -> Result<&'static str, EngineError> {
        Ok(
            "CALL db.index.vector.queryNodes('embedding', $vector, 10) YIELD node, score RETURN node.id AS id",
        )
    }
}

impl Session for RhizoSession<'_> {
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
        self.transaction = Some(self.database.begin_write().map_err(failed)?);
        Ok(())
    }

    fn commit(&mut self) -> Result<(), EngineError> {
        match self.transaction.take() {
            Some(transaction) => transaction.commit().map_err(failed),
            None => Ok(()),
        }
    }
}

impl RhizoSession<'_> {
    fn walk(
        &mut self,
        query: &Query<'_>,
        mut ids: Option<&mut Vec<i64>>,
    ) -> Result<Fingerprint, EngineError> {
        let parameters = parameters(query);
        let result = match self.transaction.as_mut() {
            Some(transaction) => transaction.execute(query.text, &parameters),
            None => self.database.execute(query.text, &parameters),
        }
        .map_err(failed)?;
        let mut fingerprint = Fingerprint::default();
        for row in result.rows() {
            if let (Some(ids), Some(Value::Integer(id))) = (ids.as_deref_mut(), row.first()) {
                ids.push(*id);
            }
            let mut hasher = RowHasher::default();
            row.iter().for_each(|value| hash(&mut hasher, value));
            fingerprint.absorb(hasher);
        }
        Ok(fingerprint)
    }
}

fn poisoned() -> EngineError {
    EngineError::Failed("the prepared input lock is poisoned".to_owned())
}

fn failed(error: rhizodb::Error) -> EngineError {
    EngineError::Failed(error.to_string())
}

fn parameters(query: &Query<'_>) -> Parameters {
    query
        .params
        .iter()
        .map(|(name, param)| ((*name).to_owned(), value(param)))
        .collect()
}

fn value(param: &Param) -> Value {
    match param {
        Param::Int(value) => Value::Integer(*value),
        Param::Text(value) => Value::String(value.clone()),
        Param::Floats(values) => Value::List(values.iter().copied().map(Value::Float).collect()),
        Param::NullInt => Value::Null,
        Param::Rows(rows) => Value::List(rows.iter().map(|row| row_value(row)).collect()),
    }
}

fn row_value(row: &[(&'static str, Param)]) -> Value {
    Value::Map(
        row.iter()
            .map(|(name, param)| ((*name).to_owned(), value(param)))
            .collect(),
    )
}

fn hash(hasher: &mut RowHasher, value: &Value) {
    match value {
        Value::Null => hasher.null(),
        Value::Boolean(value) => hasher.boolean(*value),
        Value::Integer(value) => hasher.int(*value),
        Value::Float(value) => hasher.float(*value),
        Value::String(value) => hasher.text(value),
        Value::List(items) => {
            hasher.open_list();
            items.iter().for_each(|item| hash(hasher, item));
            hasher.close_list();
        }
        other => hasher.text(&format!("{other:?}")),
    }
}

fn person_rows(data: &Dataset) -> Vec<Vec<(&'static str, Value)>> {
    data.people
        .iter()
        .map(|person| {
            let mut row = vec![
                ("id", Value::Integer(person.id)),
                ("name", Value::String(person.name.clone())),
                ("bio", Value::String(person.bio.clone())),
                (
                    "embedding",
                    Value::List(person.embedding.iter().copied().map(Value::Float).collect()),
                ),
            ];
            if let Some(age) = person.age {
                row.push(("age", Value::Integer(age)));
            }
            row
        })
        .collect()
}

fn city_rows(data: &Dataset) -> Vec<Vec<(&'static str, Value)>> {
    data.cities
        .iter()
        .map(|city| {
            vec![
                ("id", Value::Integer(city.id)),
                ("name", Value::String(city.name.clone())),
            ]
        })
        .collect()
}

fn rows_parameter(rows: Vec<Vec<(&'static str, Value)>>) -> Parameters {
    let rows = rows
        .into_iter()
        .map(|row| {
            Value::Map(
                row.into_iter()
                    .map(|(name, value)| (name.to_owned(), value))
                    .collect(),
            )
        })
        .collect();
    Parameters::from([("rows", Value::List(rows))])
}

fn create_nodes(
    transaction: &mut WriteTransaction<'_>,
    statement: &str,
    batches: &[Parameters],
) -> Result<Vec<Value>, EngineError> {
    let mut handles = Vec::new();
    for batch in batches {
        let result = transaction.execute(statement, batch).map_err(failed)?;
        for row in result.rows() {
            if let Some(id @ Value::Integer(_)) = row.first() {
                handles.push(id.clone());
            }
        }
    }
    Ok(handles)
}

fn create_edges(
    transaction: &mut WriteTransaction<'_>,
    statement: &str,
    batches: Vec<EdgeBatch>,
    sources: &[Value],
    targets: &[Value],
) -> Result<(), EngineError> {
    let ends = |positions: &[usize], handles: &[Value]| {
        Value::List(
            positions
                .iter()
                .map(|at| handles.get(*at).cloned().unwrap_or(Value::Null))
                .collect(),
        )
    };
    for batch in batches {
        let mut parameters = Parameters::new()
            .with("from", ends(&batch.from, sources))
            .with("to", ends(&batch.to, targets));
        if let Some(since) = batch.since {
            parameters.insert("since", since);
        }
        transaction
            .execute(statement, &parameters)
            .map_err(failed)?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "rhizo.test.rs"]
mod tests;
