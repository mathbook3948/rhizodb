use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;

use neo4rs::BoltFloat;
use neo4rs::BoltInteger;
use neo4rs::BoltList;
use neo4rs::BoltMap;
use neo4rs::BoltNull;
use neo4rs::BoltString;
use neo4rs::BoltType;
use neo4rs::ConfigBuilder;
use neo4rs::Graph;
use neo4rs::Query as BoltQuery;
use neo4rs::Txn;
use tokio::runtime::Runtime;

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

const NODE_BATCH: usize = 2_000;
const EDGE_BATCH: usize = 10_000;

const CREATE_PERSONS: &str = "UNWIND $rows AS r CREATE (:Person {id: r.id, name: r.name, age: r.age, bio: r.bio, embedding: r.embedding})";
const CREATE_CITIES: &str = "UNWIND $rows AS r CREATE (:City {id: r.id, name: r.name})";
const CREATE_KNOWS: &str = "UNWIND $rows AS r MATCH (a:Person {id: r.src}), (b:Person {id: r.dst}) CREATE (a)-[:KNOWS {since: r.since}]->(b)";
const CREATE_LIVES_IN: &str = "UNWIND $rows AS r MATCH (a:Person {id: r.src}), (b:City {id: r.dst}) CREATE (a)-[:LIVES_IN]->(b)";

/// Neo4j and Memgraph both speak Bolt and Cypher, so one module reaches both through `neo4rs`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flavor {
    Neo4j,
    Memgraph,
}

/// A server reached over Bolt at the URL it was started with.
#[derive(Clone, Debug)]
pub struct BoltEngine {
    flavor: Flavor,
    url: Option<String>,
}

impl BoltEngine {
    /// The engine for `flavor`, which is skipped without a URL.
    pub fn new(flavor: Flavor, url: Option<String>) -> BoltEngine {
        BoltEngine { flavor, url }
    }
}

type Batches = Vec<(&'static str, BoltType)>;

struct BoltInstance {
    url: String,
    flavor: Flavor,
    runtime: Arc<Runtime>,
    graph: Graph,
    prepared: Mutex<Option<Batches>>,
}

impl std::fmt::Debug for BoltInstance {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BoltInstance")
            .field("flavor", &self.flavor)
            .finish_non_exhaustive()
    }
}

struct BoltSession<'a> {
    runtime: &'a Runtime,
    graph: &'a Graph,
    transaction: Option<Txn>,
}

fn failed(error: impl std::fmt::Display) -> EngineError {
    EngineError::Failed(error.to_string())
}

fn runtime() -> Result<Arc<Runtime>, EngineError> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map(Arc::new)
        .map_err(failed)
}

fn connect(url: &str, flavor: Flavor) -> Result<(Arc<Runtime>, Graph), EngineError> {
    let runtime = runtime()?;
    let mut config = ConfigBuilder::default().uri(url).user("").password("");
    if flavor == Flavor::Memgraph {
        config = config.db("memgraph");
    }
    let config = config.build().map_err(failed)?;
    let graph = runtime
        .block_on(Graph::connect(config))
        .map_err(|error| EngineError::Failed(format!("cannot connect to {url}: {error}")))?;
    Ok((runtime, graph))
}

impl Engine for BoltEngine {
    fn kind(&self) -> EngineKind {
        match self.flavor {
            Flavor::Neo4j => EngineKind::Neo4j,
            Flavor::Memgraph => EngineKind::Memgraph,
        }
    }

    fn dialect(&self) -> Dialect {
        Dialect::Cypher
    }

    fn open(&self, _dir: &Path) -> Result<Box<dyn Instance>, EngineError> {
        let url = self.url.as_deref().ok_or_else(|| {
            EngineError::Unsupported(format!("no server URL given for {}", self.kind().name()))
        })?;
        let (runtime, graph) = connect(url, self.flavor)?;
        Ok(Box::new(BoltInstance {
            url: url.to_owned(),
            flavor: self.flavor,
            runtime,
            graph,
            prepared: Mutex::new(None),
        }))
    }

    fn config(&self) -> serde_json::Value {
        let version = self.url.as_deref().and_then(|url| {
            let (runtime, graph) = connect(url, self.flavor).ok()?;
            let query = match self.flavor {
                Flavor::Neo4j => "CALL dbms.components() YIELD name, versions, edition RETURN name + ' ' + versions[0] + ' ' + edition AS c0",
                Flavor::Memgraph => "SHOW VERSION",
            };
            runtime.block_on(async {
                let mut stream = graph.execute(BoltQuery::new(query.to_owned())).await.ok()?;
                let row = stream.next().await.ok()??;
                match self.flavor {
                    Flavor::Neo4j => row.get::<String>("c0").ok(),
                    Flavor::Memgraph => row.get::<String>("version").ok(),
                }
            })
        });
        serde_json::json!({
            "version": version,
            "pinned": env!("PIN_NEO4RS"),
            "driver": "neo4rs over Bolt",
            "url": self.url,
            "storage": match self.flavor {
                Flavor::Neo4j => "file, store directory not sized",
                Flavor::Memgraph => "in-memory transactional storage",
            },
            "load": "UNWIND batches, index or constraint on :Person(id) and :City(id) before load",
            "mutating_workloads": "the graph is cleared and loaded again before each, untimed",
        })
    }
}

fn bolt_value(param: &Param) -> BoltType {
    match param {
        Param::Int(value) => BoltType::Integer(BoltInteger::new(*value)),
        Param::Text(value) => BoltType::String(BoltString::new(value)),
        Param::Floats(values) => BoltType::List(BoltList::from(
            values
                .iter()
                .map(|&value| BoltType::Float(BoltFloat::new(value)))
                .collect::<Vec<_>>(),
        )),
        Param::NullInt => BoltType::Null(BoltNull),
        Param::Rows(rows) => BoltType::List(BoltList::from(
            rows.iter()
                .map(|row| {
                    BoltType::Map(
                        row.iter()
                            .map(|(name, param)| (BoltString::new(name), bolt_value(param)))
                            .collect::<BoltMap>(),
                    )
                })
                .collect::<Vec<_>>(),
        )),
    }
}

fn int(value: i64) -> BoltType {
    BoltType::Integer(BoltInteger::new(value))
}

fn text(value: &str) -> BoltType {
    BoltType::String(BoltString::new(value))
}

fn map(fields: Vec<(&str, BoltType)>) -> BoltType {
    BoltType::Map(
        fields
            .into_iter()
            .map(|(name, value)| (BoltString::new(name), value))
            .collect::<BoltMap>(),
    )
}

fn list(values: Vec<BoltType>) -> BoltType {
    BoltType::List(BoltList::from(values))
}

impl BoltInstance {
    fn run(&self, statement: &str) -> Result<(), EngineError> {
        self.runtime
            .block_on(self.graph.run(BoltQuery::new(statement.to_owned())))
            .map_err(failed)
    }

    fn column(&self, statement: &str) -> Result<Vec<String>, EngineError> {
        self.runtime.block_on(async {
            let mut stream = self
                .graph
                .execute(BoltQuery::new(statement.to_owned()))
                .await
                .map_err(failed)?;
            let mut found = Vec::new();
            while let Some(row) = stream.next().await.map_err(failed)? {
                if let Ok(value) = row.get::<String>("c0") {
                    found.push(value);
                }
            }
            Ok(found)
        })
    }

    fn rows(&self, statement: &str, keys: &[&str]) -> Result<Vec<Vec<BoltType>>, EngineError> {
        self.runtime.block_on(async {
            let mut stream = self
                .graph
                .execute(BoltQuery::new(statement.to_owned()))
                .await
                .map_err(failed)?;
            let mut found = Vec::new();
            while let Some(row) = stream.next().await.map_err(failed)? {
                let values = keys
                    .iter()
                    .filter_map(|key| row.get::<BoltType>(key).ok())
                    .collect();
                found.push(values);
            }
            Ok(found)
        })
    }

    fn feature(&self, statement: &str) -> Result<(), EngineError> {
        let (runtime, graph) = connect(&self.url, self.flavor)?;
        runtime
            .block_on(graph.run(BoltQuery::new(statement.to_owned())))
            .map_err(|error| EngineError::Unsupported(format!("{}: {error}", self.flavor.name())))
    }
}

impl Flavor {
    fn name(self) -> &'static str {
        match self {
            Flavor::Neo4j => "neo4j",
            Flavor::Memgraph => "memgraph",
        }
    }
}

impl Instance for BoltInstance {
    fn session(&self) -> Result<Box<dyn Session + '_>, EngineError> {
        Ok(Box::new(BoltSession {
            runtime: &self.runtime,
            graph: &self.graph,
            transaction: None,
        }))
    }

    fn create_schema(&self) -> Result<(), EngineError> {
        match self.flavor {
            Flavor::Neo4j => {
                self.run("CREATE CONSTRAINT person_id IF NOT EXISTS FOR (p:Person) REQUIRE p.id IS UNIQUE")?;
                self.run(
                    "CREATE CONSTRAINT city_id IF NOT EXISTS FOR (c:City) REQUIRE c.id IS UNIQUE",
                )?;
                self.run("CALL db.awaitIndexes(300)")
            }
            Flavor::Memgraph => {
                self.run("CREATE INDEX ON :Person(id)")?;
                self.run("CREATE INDEX ON :City(id)")
            }
        }
    }

    fn prepare_bulk_load(&self, data: &Dataset, _scratch: &Path) -> Result<(), EngineError> {
        let mut batches: Batches = Vec::new();
        for people in data.people.chunks(NODE_BATCH) {
            let rows = people
                .iter()
                .map(|person| {
                    map(vec![
                        ("id", int(person.id)),
                        ("name", text(&person.name)),
                        ("age", person.age.map_or(BoltType::Null(BoltNull), int)),
                        ("bio", text(&person.bio)),
                        (
                            "embedding",
                            bolt_value(&Param::Floats(person.embedding.clone())),
                        ),
                    ])
                })
                .collect();
            batches.push((CREATE_PERSONS, list(rows)));
        }
        let cities = data
            .cities
            .iter()
            .map(|city| map(vec![("id", int(city.id)), ("name", text(&city.name))]))
            .collect();
        batches.push((CREATE_CITIES, list(cities)));
        for edges in data.knows.chunks(EDGE_BATCH) {
            let rows = edges
                .iter()
                .map(|edge| {
                    map(vec![
                        ("src", int(edge.from)),
                        ("dst", int(edge.to)),
                        ("since", int(edge.since)),
                    ])
                })
                .collect();
            batches.push((CREATE_KNOWS, list(rows)));
        }
        for edges in data.lives_in.chunks(EDGE_BATCH) {
            let rows = edges
                .iter()
                .map(|edge| map(vec![("src", int(edge.person)), ("dst", int(edge.city))]))
                .collect();
            batches.push((CREATE_LIVES_IN, list(rows)));
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
        for (statement, rows) in batches {
            let query = BoltQuery::new(statement.to_owned()).param("rows", rows);
            self.runtime
                .block_on(self.graph.run(query))
                .map_err(failed)?;
        }
        Ok(())
    }

    fn reset(&self) -> Result<(), EngineError> {
        match self.flavor {
            Flavor::Neo4j => {
                self.run(
                    "MATCH (n) CALL { WITH n DETACH DELETE n } IN TRANSACTIONS OF 10000 ROWS",
                )?;
                for name in self.column("SHOW CONSTRAINTS YIELD name RETURN name AS c0")? {
                    self.run(&format!("DROP CONSTRAINT {name}"))?;
                }
                for name in self.column(
                    "SHOW INDEXES YIELD name, type WHERE type <> 'LOOKUP' RETURN name AS c0",
                )? {
                    self.run(&format!("DROP INDEX {name}"))?;
                }
                Ok(())
            }
            Flavor::Memgraph => {
                self.run("MATCH (n) DETACH DELETE n")?;
                for row in self.rows("SHOW INDEX INFO", &["index type", "label", "property"])? {
                    if let [BoltType::String(kind), BoltType::String(label), property] = &row[..] {
                        let target = match (kind.value.as_str(), property) {
                            ("label", _) => format!(":{}", label.value),
                            (_, BoltType::List(names)) => match names.value.first() {
                                Some(BoltType::String(name)) => {
                                    format!(":{}({})", label.value, name.value)
                                }
                                _ => continue,
                            },
                            _ => continue,
                        };
                        self.run(&format!("DROP INDEX ON {target}"))?;
                    }
                }
                for row in self.rows("SHOW VECTOR INDEX INFO", &["index_name"])? {
                    if let [BoltType::String(name)] = &row[..] {
                        self.run(&format!("DROP VECTOR INDEX {}", name.value))?;
                    }
                }
                Ok(())
            }
        }
    }

    fn create_keyword_index(&self) -> Result<Built, EngineError> {
        match self.flavor {
            Flavor::Neo4j => {
                self.feature(
                    "CREATE FULLTEXT INDEX bio_text IF NOT EXISTS FOR (p:Person) ON EACH [p.bio]",
                )?;
                self.feature("CALL db.awaitIndexes(600)")?;
            }
            Flavor::Memgraph => self.feature("CREATE TEXT INDEX bio_text ON :Person")?,
        }
        Ok(Built::Index)
    }

    fn create_vector_index(&self) -> Result<Built, EngineError> {
        match self.flavor {
            Flavor::Neo4j => {
                self.feature("CREATE VECTOR INDEX embedding_index IF NOT EXISTS FOR (p:Person) ON (p.embedding) OPTIONS {indexConfig: {`vector.dimensions`: 64, `vector.similarity_function`: 'cosine'}}")?;
                self.feature("CALL db.awaitIndexes(600)")?;
            }
            Flavor::Memgraph => self.feature("CREATE VECTOR INDEX embedding_index ON :Person(embedding) WITH CONFIG {\"dimension\": 64, \"capacity\": 1000000, \"metric\": \"cos\"}")?,
        }
        Ok(Built::Index)
    }

    fn keyword_search(&self) -> Result<&'static str, EngineError> {
        Ok(match self.flavor {
            Flavor::Neo4j => {
                "CALL db.index.fulltext.queryNodes('bio_text', $term) YIELD node, score RETURN node.id AS id, score ORDER BY score DESC LIMIT 10"
            }
            Flavor::Memgraph => {
                "CALL text_search.search('bio_text', $term) YIELD node, score RETURN node.id AS id, score ORDER BY score DESC LIMIT 10"
            }
        })
    }

    fn vector_search(&self) -> Result<&'static str, EngineError> {
        Ok(match self.flavor {
            Flavor::Neo4j => {
                "CALL db.index.vector.queryNodes('embedding_index', 10, $vector) YIELD node, score RETURN node.id AS id, score ORDER BY score DESC LIMIT 10"
            }
            Flavor::Memgraph => {
                "CALL vector_search.search('embedding_index', 10, $vector) YIELD node, similarity RETURN node.id AS id, similarity ORDER BY similarity DESC LIMIT 10"
            }
        })
    }
}

fn poisoned() -> EngineError {
    EngineError::Failed("the prepared input lock is poisoned".to_owned())
}

/// Names every returned column `c0`, `c1`, and so on in order, because the driver hands back a
/// row's values by name and loses their order. A query without `RETURN` stays as it is.
pub fn alias_columns(text: &str) -> String {
    let Some(start) = text.rfind("RETURN ") else {
        return text.to_owned();
    };
    let head = &text[..start + "RETURN ".len()];
    let tail = &text[start + "RETURN ".len()..];
    let (distinct, tail) = match tail.strip_prefix("DISTINCT ") {
        Some(rest) => ("DISTINCT ", rest),
        None => ("", tail),
    };
    let end = [" ORDER BY", " SKIP ", " LIMIT "]
        .iter()
        .filter_map(|clause| top_level(tail, clause))
        .min()
        .unwrap_or(tail.len());
    let (items, rest) = tail.split_at(end);
    let mut aliased = Vec::new();
    let mut from = 0;
    let mut depth = 0_i32;
    let mut quote: Option<char> = None;
    let bytes: Vec<(usize, char)> = items.char_indices().collect();
    let mut pieces = Vec::new();
    for &(index, character) in &bytes {
        match (quote, character) {
            (Some(open), found) if found == open => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"') => quote = Some(character),
            (None, '(' | '[' | '{') => depth += 1,
            (None, ')' | ']' | '}') => depth -= 1,
            (None, ',') if depth == 0 => {
                pieces.push(&items[from..index]);
                from = index + 1;
            }
            _ => {}
        }
    }
    pieces.push(&items[from..]);
    for (position, piece) in pieces.iter().enumerate() {
        let piece = piece.trim();
        let expression = match top_level(piece, " AS ") {
            Some(at) => &piece[..at],
            None => piece,
        };
        aliased.push(format!("{expression} AS c{position}"));
    }
    format!("{head}{distinct}{}{rest}", aliased.join(", "))
}

fn top_level(text: &str, needle: &str) -> Option<usize> {
    let mut depth = 0_i32;
    let mut quote: Option<char> = None;
    let mut found = None;
    for (index, character) in text.char_indices() {
        match (quote, character) {
            (Some(open), seen) if seen == open => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"') => quote = Some(character),
            (None, '(' | '[' | '{') => depth += 1,
            (None, ')' | ']' | '}') => depth -= 1,
            _ => {}
        }
        if depth == 0 && quote.is_none() && text[index..].starts_with(needle) {
            found = Some(index);
        }
    }
    found
}

impl Session for BoltSession<'_> {
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
        self.transaction = Some(
            self.runtime
                .block_on(self.graph.start_txn())
                .map_err(failed)?,
        );
        Ok(())
    }

    fn commit(&mut self) -> Result<(), EngineError> {
        match self.transaction.take() {
            Some(transaction) => self.runtime.block_on(transaction.commit()).map_err(failed),
            None => Ok(()),
        }
    }
}

impl BoltSession<'_> {
    fn walk(
        &mut self,
        query: &Query<'_>,
        mut ids: Option<&mut Vec<i64>>,
    ) -> Result<Fingerprint, EngineError> {
        let mut bolt = BoltQuery::new(alias_columns(query.text));
        for (name, param) in &query.params {
            bolt = bolt.param(name, bolt_value(param));
        }
        let runtime = self.runtime;
        let graph = self.graph;
        let transaction = self.transaction.as_mut();
        runtime.block_on(async move {
            let mut fingerprint = Fingerprint::default();
            let mut collect = |row: neo4rs::Row| {
                let mut hasher = RowHasher::default();
                let mut column = 0;
                while let Ok(value) = row.get::<BoltType>(&format!("c{column}")) {
                    if let (0, Some(ids), BoltType::Integer(id)) =
                        (column, ids.as_deref_mut(), &value)
                    {
                        ids.push(id.value);
                    }
                    hash(&mut hasher, &value);
                    column += 1;
                }
                fingerprint.absorb(hasher);
            };
            match transaction {
                Some(transaction) => {
                    let mut stream = transaction.execute(bolt).await.map_err(failed)?;
                    while let Some(row) = stream.next(transaction.handle()).await.map_err(failed)? {
                        collect(row);
                    }
                }
                None => {
                    let mut stream = graph.execute(bolt).await.map_err(failed)?;
                    while let Some(row) = stream.next().await.map_err(failed)? {
                        collect(row);
                    }
                }
            }
            Ok(fingerprint)
        })
    }
}

fn hash(hasher: &mut RowHasher, value: &BoltType) {
    match value {
        BoltType::Null(_) => hasher.null(),
        BoltType::Boolean(value) => hasher.boolean(value.value),
        BoltType::Integer(value) => hasher.int(value.value),
        BoltType::Float(value) => hasher.float(value.value),
        BoltType::String(value) => hasher.text(&value.value),
        BoltType::List(items) => {
            hasher.open_list();
            items.value.iter().for_each(|item| hash(hasher, item));
            hasher.close_list();
        }
        other => hasher.text(&format!("{other:?}")),
    }
}

#[cfg(test)]
#[path = "bolt.test.rs"]
mod tests;
