use std::collections::BTreeMap;
use std::path::Path;

use rhizodb::Database;
use rhizodb::IndexTarget;
use rhizodb::Node;
use rhizodb::Parameters;
use rhizodb::Value;
use rhizodb::VectorMetric;
use rhizodb::WriteTransaction;

use crate::data::Dataset;
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
const FILE_NAME: &str = "rhizodb.redb";

const CREATE_PERSONS: &str = "UNWIND $rows AS r CREATE (p:Person {id: r.id, name: r.name, age: r.age, bio: r.bio, embedding: r.embedding}) RETURN p";
const CREATE_CITIES: &str = "UNWIND $rows AS r CREATE (c:City {id: r.id, name: r.name}) RETURN c";
const CREATE_KNOWS: &str = "UNWIND $rows AS r WITH r.a AS a, r.b AS b, r.since AS since CREATE (a)-[:KNOWS {since: since}]->(b)";
const CREATE_LIVES_IN: &str =
    "UNWIND $rows AS r WITH r.a AS a, r.b AS b CREATE (a)-[:LIVES_IN]->(b)";

/// RhizoDB, opened as a file.
#[derive(Clone, Copy, Debug)]
pub struct RhizoEngine;

#[derive(Debug)]
struct RhizoInstance {
    database: Database,
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
        Ok(Box::new(RhizoInstance { database }))
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

    fn prepare_bulk_load(&self, _data: &Dataset, _scratch: &Path) -> Result<(), EngineError> {
        Ok(())
    }

    fn bulk_load(&self, data: &Dataset, _scratch: &Path) -> Result<(), EngineError> {
        let mut transaction = self.database.begin_write().map_err(failed)?;
        let persons = create_nodes(&mut transaction, CREATE_PERSONS, &person_rows(data))?;
        let cities = create_nodes(&mut transaction, CREATE_CITIES, &city_rows(data))?;
        let knows = data.knows.iter().map(|edge| {
            vec![
                ("a", handle(&persons, edge.from)),
                ("b", handle(&persons, edge.to)),
                ("since", Value::Integer(edge.since)),
            ]
        });
        create_edges(&mut transaction, CREATE_KNOWS, knows)?;
        let lives_in = data.lives_in.iter().map(|edge| {
            vec![
                ("a", handle(&persons, edge.person)),
                ("b", handle(&cities, edge.city)),
            ]
        });
        create_edges(&mut transaction, CREATE_LIVES_IN, lives_in)?;
        transaction.commit().map_err(failed)
    }

    fn create_keyword_index(&self) -> Result<(), EngineError> {
        self.database
            .create_keyword_index("bio", IndexTarget::Label("Person"), &["bio"])
            .map_err(failed)
    }

    fn create_vector_index(&self) -> Result<(), EngineError> {
        self.database
            .create_vector_index(
                "embedding",
                IndexTarget::Label("Person"),
                "embedding",
                64,
                VectorMetric::Cosine,
            )
            .map_err(failed)
    }

    fn keyword_search(&self) -> &'static str {
        "CALL db.index.keyword.queryNodes('bio', $term, 10) YIELD node, score RETURN node.id AS id"
    }

    fn vector_search(&self) -> &'static str {
        "CALL db.index.vector.queryNodes('embedding', $vector, 10) YIELD node, score RETURN node.id AS id"
    }
}

impl Session for RhizoSession<'_> {
    fn prepare(&mut self, _text: &str) -> Result<(), EngineError> {
        Ok(())
    }

    fn run(&mut self, query: &Query<'_>) -> Result<Fingerprint, EngineError> {
        let parameters = parameters(query);
        let result = match self.transaction.as_mut() {
            Some(transaction) => transaction.execute(query.text, &parameters),
            None => self.database.execute(query.text, &parameters),
        }
        .map_err(failed)?;
        let mut fingerprint = Fingerprint::default();
        for row in result.rows() {
            let mut hasher = RowHasher::default();
            row.iter().for_each(|value| hash(&mut hasher, value));
            fingerprint.absorb(hasher);
        }
        Ok(fingerprint)
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
    BTreeMap::from([("rows".to_owned(), Value::List(rows))])
}

fn create_nodes(
    transaction: &mut WriteTransaction<'_>,
    statement: &str,
    rows: &[Vec<(&'static str, Value)>],
) -> Result<Vec<Value>, EngineError> {
    let mut handles = Vec::with_capacity(rows.len());
    for batch in rows.chunks(NODE_BATCH) {
        let result = transaction
            .execute(statement, &rows_parameter(batch.to_vec()))
            .map_err(failed)?;
        for row in result.rows() {
            if let Some(Value::Node(node)) = row.first() {
                handles.push(Value::Node(Node {
                    id: node.id,
                    labels: node.labels.clone(),
                    properties: BTreeMap::new(),
                }));
            }
        }
    }
    Ok(handles)
}

fn create_edges(
    transaction: &mut WriteTransaction<'_>,
    statement: &str,
    edges: impl Iterator<Item = Vec<(&'static str, Value)>>,
) -> Result<(), EngineError> {
    let mut batch = Vec::with_capacity(EDGE_BATCH);
    for edge in edges {
        batch.push(edge);
        if batch.len() == EDGE_BATCH {
            let rows = rows_parameter(std::mem::take(&mut batch));
            transaction.execute(statement, &rows).map_err(failed)?;
        }
    }
    if !batch.is_empty() {
        transaction
            .execute(statement, &rows_parameter(batch))
            .map_err(failed)?;
    }
    Ok(())
}

fn handle(handles: &[Value], id: i64) -> Value {
    usize::try_from(id - 1)
        .ok()
        .and_then(|index| handles.get(index))
        .cloned()
        .unwrap_or(Value::Null)
}

#[cfg(test)]
#[path = "rhizo.test.rs"]
mod tests;
