use std::path::Path;
use std::sync::Mutex;

use falkordb::FalkorClientBuilder;
use falkordb::FalkorConnectionInfo;
use falkordb::FalkorSyncClient;
use falkordb::FalkorValue;
use falkordb::SyncGraph;

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

const GRAPH: &str = "bench";
const NODE_BATCH: usize = 1_000;
const EDGE_BATCH: usize = 10_000;

const CREATE_PERSONS: &str = "UNWIND $rows AS r CREATE (:Person {id: r.id, name: r.name, age: r.age, bio: r.bio, embedding: vecf32(r.embedding)})";
const CREATE_CITIES: &str = "UNWIND $rows AS r CREATE (:City {id: r.id, name: r.name})";
const CREATE_KNOWS: &str = "UNWIND $rows AS r MATCH (a:Person {id: r.src}), (b:Person {id: r.dst}) CREATE (a)-[:KNOWS {since: r.since}]->(b)";
const CREATE_LIVES_IN: &str = "UNWIND $rows AS r MATCH (a:Person {id: r.src}), (b:City {id: r.dst}) CREATE (a)-[:LIVES_IN]->(b)";

/// FalkorDB, a module in Redis, reached over the Redis protocol with the `falkordb` crate. It
/// keeps the graph in memory and has no persistence here.
#[derive(Clone, Debug)]
pub struct FalkorEngine {
    url: Option<String>,
}

impl FalkorEngine {
    /// The engine, which is skipped without a URL.
    pub fn new(url: Option<String>) -> FalkorEngine {
        FalkorEngine { url }
    }

    fn client(&self) -> Result<FalkorSyncClient, EngineError> {
        let url = self.url.as_deref().ok_or_else(|| {
            EngineError::Unsupported("no server URL given for falkordb".to_owned())
        })?;
        let info = FalkorConnectionInfo::try_from(url).map_err(failed)?;
        FalkorClientBuilder::new()
            .with_connection_info(info)
            .with_num_connections(std::num::NonZeroU8::new(8).unwrap_or(std::num::NonZeroU8::MIN))
            .build()
            .map_err(|error| EngineError::Failed(format!("cannot connect to {url}: {error}")))
    }
}

type Batches = Vec<(&'static str, String)>;

struct FalkorInstance {
    client: FalkorSyncClient,
    prepared: Mutex<Option<Batches>>,
}

impl std::fmt::Debug for FalkorInstance {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FalkorInstance")
            .finish_non_exhaustive()
    }
}

struct FalkorSession {
    graph: SyncGraph,
}

fn failed(error: impl std::fmt::Display) -> EngineError {
    EngineError::Failed(error.to_string())
}

impl Engine for FalkorEngine {
    fn kind(&self) -> EngineKind {
        EngineKind::Falkordb
    }

    fn dialect(&self) -> Dialect {
        Dialect::Cypher
    }

    fn open(&self, _dir: &Path) -> Result<Box<dyn Instance>, EngineError> {
        Ok(Box::new(FalkorInstance {
            client: self.client()?,
            prepared: Mutex::new(None),
        }))
    }

    fn config(&self) -> serde_json::Value {
        let version = self.client().ok().and_then(|client| {
            let info = client.redis_info(Some("server")).ok()?;
            Some(format!("{info:?}"))
        });
        serde_json::json!({
            "version": version.map(|text| text.chars().take(400).collect::<String>()),
            "pinned": env!("PIN_FALKORDB"),
            "driver": "falkordb crate over the Redis protocol",
            "url": self.url,
            "storage": "in-memory, no persistence",
            "load": "UNWIND batches, index on :Person(id) and :City(id) before load",
            "mutating_workloads": "the graph is deleted and loaded again before each, untimed",
        })
    }
}

fn literal(param: &Param) -> String {
    match param {
        Param::Int(value) => value.to_string(),
        Param::Text(value) => quote(value),
        Param::Floats(values) => format!(
            "[{}]",
            values
                .iter()
                .map(|value| format!("{value:?}"))
                .collect::<Vec<_>>()
                .join(",")
        ),
        Param::Ints(values) => format!(
            "[{}]",
            values
                .iter()
                .map(i64::to_string)
                .collect::<Vec<_>>()
                .join(",")
        ),
        Param::NullInt => "null".to_owned(),
        Param::Rows(rows) => format!(
            "[{}]",
            rows.iter()
                .map(|row| format!(
                    "{{{}}}",
                    row.iter()
                        .map(|(name, param)| format!("{name}:{}", literal(param)))
                        .collect::<Vec<_>>()
                        .join(",")
                ))
                .collect::<Vec<_>>()
                .join(",")
        ),
    }
}

fn quote(text: &str) -> String {
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push('\'');
    for character in text.chars() {
        if character == '\\' || character == '\'' {
            quoted.push('\\');
        }
        quoted.push(character);
    }
    quoted.push('\'');
    quoted
}

impl FalkorInstance {
    fn graph(&self) -> SyncGraph {
        self.client.select_graph(GRAPH)
    }

    fn run(&self, statement: &str) -> Result<(), EngineError> {
        self.graph().query(statement).execute().map_err(failed)?;
        Ok(())
    }

    fn feature(&self, statement: &str) -> Result<(), EngineError> {
        self.run(statement)
            .map_err(|error| EngineError::Unsupported(format!("falkordb: {error}")))
    }
}

impl Instance for FalkorInstance {
    fn session(&self) -> Result<Box<dyn Session + '_>, EngineError> {
        Ok(Box::new(FalkorSession {
            graph: self.graph(),
        }))
    }

    fn create_schema(&self) -> Result<(), EngineError> {
        self.run("CREATE INDEX FOR (p:Person) ON (p.id)")?;
        self.run("CREATE INDEX FOR (c:City) ON (c.id)")
    }

    fn prepare_bulk_load(&self, data: &Dataset, _scratch: &Path) -> Result<(), EngineError> {
        let mut batches: Batches = Vec::new();
        for people in data.people.chunks(NODE_BATCH) {
            let rows = people
                .iter()
                .map(|person| {
                    vec![
                        ("id", Param::Int(person.id)),
                        ("name", Param::Text(person.name.clone())),
                        ("age", person.age.map_or(Param::NullInt, Param::Int)),
                        ("bio", Param::Text(person.bio.clone())),
                        ("embedding", Param::Floats(person.embedding.clone())),
                    ]
                })
                .collect();
            batches.push((CREATE_PERSONS, literal(&Param::Rows(rows))));
        }
        let cities = data
            .cities
            .iter()
            .map(|city| {
                vec![
                    ("id", Param::Int(city.id)),
                    ("name", Param::Text(city.name.clone())),
                ]
            })
            .collect();
        batches.push((CREATE_CITIES, literal(&Param::Rows(cities))));
        for edges in data.knows.chunks(EDGE_BATCH) {
            let rows = edges
                .iter()
                .map(|edge| {
                    vec![
                        ("src", Param::Int(edge.from)),
                        ("dst", Param::Int(edge.to)),
                        ("since", Param::Int(edge.since)),
                    ]
                })
                .collect();
            batches.push((CREATE_KNOWS, literal(&Param::Rows(rows))));
        }
        for edges in data.lives_in.chunks(EDGE_BATCH) {
            let rows = edges
                .iter()
                .map(|edge| {
                    vec![
                        ("src", Param::Int(edge.person)),
                        ("dst", Param::Int(edge.city)),
                    ]
                })
                .collect();
            batches.push((CREATE_LIVES_IN, literal(&Param::Rows(rows))));
        }
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
        let mut graph = self.graph();
        for (statement, rows) in batches {
            graph
                .query(statement)
                .with_raw_param("rows", rows)
                .execute()
                .map_err(failed)?;
        }
        Ok(())
    }

    fn reset(&self) -> Result<(), EngineError> {
        let mut graph = self.graph();
        match graph.delete() {
            Ok(()) => Ok(()),
            Err(error) if error.to_string().contains("empty key") => Ok(()),
            Err(error) => Err(failed(error)),
        }
    }

    fn create_keyword_index(&self) -> Result<Built, EngineError> {
        self.feature("CALL db.idx.fulltext.createNodeIndex('Person', 'bio')")?;
        Ok(Built::Index)
    }

    fn create_vector_index(&self) -> Result<Built, EngineError> {
        self.feature("CREATE VECTOR INDEX FOR (p:Person) ON (p.embedding) OPTIONS {dimension: 64, similarityFunction: 'cosine'}")?;
        let zeros = literal(&Param::Floats(vec![0.0; 64]));
        let probe = format!(
            "CALL db.idx.vector.queryNodes('Person', 'embedding', 1, vecf32({zeros})) YIELD node RETURN count(node)"
        );
        let mut ready = self.run(&probe);
        for _ in 0..500 {
            if ready.is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
            ready = self.run(&probe);
        }
        ready?;
        Ok(Built::Index)
    }

    fn keyword_search(&self) -> Result<&'static str, EngineError> {
        Ok(
            "CALL db.idx.fulltext.queryNodes('Person', $term) YIELD node, score RETURN node.id AS id, score ORDER BY score DESC LIMIT 10",
        )
    }

    fn vector_search(&self) -> Result<&'static str, EngineError> {
        Ok(
            "CALL db.idx.vector.queryNodes('Person', 'embedding', 10, vecf32($vector)) YIELD node, score RETURN node.id AS id, score ORDER BY score ASC LIMIT 10",
        )
    }
}

fn poisoned() -> EngineError {
    EngineError::Failed("the prepared input lock is poisoned".to_owned())
}

impl Session for FalkorSession {
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
        Ok(())
    }

    fn commit(&mut self) -> Result<(), EngineError> {
        Ok(())
    }
}

impl FalkorSession {
    fn walk(
        &mut self,
        query: &Query<'_>,
        mut ids: Option<&mut Vec<i64>>,
    ) -> Result<Fingerprint, EngineError> {
        let mut builder = self.graph.query(query.text);
        for (name, param) in &query.params {
            builder = builder.with_raw_param(name, literal(param));
        }
        let result = builder.execute().map_err(failed)?;
        let mut fingerprint = Fingerprint::default();
        for row in result.data {
            let values = row.map_err(failed)?.into_values();
            if let (Some(ids), Some(FalkorValue::I64(id))) = (ids.as_deref_mut(), values.first()) {
                ids.push(*id);
            }
            let mut hasher = RowHasher::default();
            values.iter().for_each(|value| hash(&mut hasher, value));
            fingerprint.absorb(hasher);
        }
        Ok(fingerprint)
    }
}

fn hash(hasher: &mut RowHasher, value: &FalkorValue) {
    match value {
        FalkorValue::None => hasher.null(),
        FalkorValue::Bool(value) => hasher.boolean(*value),
        FalkorValue::I64(value) => hasher.int(*value),
        FalkorValue::F64(value) => hasher.float(*value),
        FalkorValue::String(value) => hasher.text(value),
        FalkorValue::Array(items) => {
            hasher.open_list();
            items.iter().for_each(|item| hash(hasher, item));
            hasher.close_list();
        }
        other => hasher.text(&format!("{other:?}")),
    }
}

#[cfg(test)]
#[path = "falkor.test.rs"]
mod tests;
