use std::collections::BTreeMap;

use crate::Database;
use crate::graph::IndexTarget;
use crate::graph::NodeId;
use crate::graph::RelationshipId;
use crate::graph::VectorMetric;
use crate::result::Node;
use crate::result::Parameters;
use crate::result::Path;
use crate::result::Relationship;
use crate::result::Value;
use crate::temporal::Date;
use crate::temporal::Duration;
use crate::temporal::Temporal;

const SETUP: &str = "UNWIND range(0, 9) AS i INSERT (:A {i: i, s: 'word ' + toString(i), v: [toFloat(i), 1.0]})-[:R {w: i}]->(:B {i: -i})";

const QUERIES: [&str; 20] = [
    "RETURN $p, $q",
    "RETURN $p = $q, $p < $q, $p + $q, $p IN $q",
    "RETURN toString($p), $p.x, $p[0], $p[$q], size($p)",
    "RETURN labels($p), type($p), nodes($p), relationships($p), properties($p), id($p)",
    "RETURN [x IN $p | x * 2], [x IN $p WHERE x > $q]",
    "UNWIND $p AS x RETURN x ORDER BY x",
    "UNWIND $p AS x RETURN DISTINCT x, count(*)",
    "MATCH (n) WHERE n.i = $p OR n = $p RETURN n",
    "MATCH (n:A {i: $p}) RETURN n",
    "MATCH (n:A) WHERE n.s STARTS WITH $p RETURN n",
    "MATCH p = ()-[r]->() WHERE p = $p OR r = $q RETURN p",
    "CALL db.index.keyword.queryNodes('words', $p, $q) YIELD node, score RETURN node, score",
    "CALL db.index.vector.queryNodes('vectors', $p, $q) YIELD node, score RETURN node, score",
    "INSERT (:A {i: $p, s: $q, v: $p})",
    "MATCH (n:A) WHERE n.i = 3 SET n.x = $p, n += $q",
    "MATCH (n:B) SET n = $p",
    "WITH $p AS n SET n.z = 1",
    "WITH $p AS n DETACH DELETE n",
    "MATCH ()-[r]->() WHERE r.w = 2 SET r.w = $p",
    "MATCH (n) WHERE n = $q DETACH DELETE n",
];

struct Bytes<'a> {
    bytes: &'a [u8],
}

impl Bytes<'_> {
    fn byte(&mut self) -> u8 {
        let (first, rest) = self.bytes.split_first().unwrap_or((&0, &[]));
        self.bytes = rest;
        *first
    }

    fn u64(&mut self) -> u64 {
        (0..8).fold(0, |number, _| number << 8 | u64::from(self.byte()))
    }

    fn text(&mut self) -> String {
        let length = usize::from(self.byte() % 8);
        let bytes: Vec<u8> = (0..length).map(|_| self.byte()).collect();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    fn id(&mut self) -> u64 {
        match self.byte() {
            255 => self.u64(),
            byte => u64::from(byte % 32),
        }
    }

    fn properties(&mut self, depth: usize) -> BTreeMap<String, Value> {
        (0..self.byte() % 3)
            .map(|_| (self.text(), self.value(depth + 1)))
            .collect()
    }

    fn node(&mut self, depth: usize) -> Node {
        Node {
            id: NodeId(self.id()),
            labels: (0..self.byte() % 3).map(|_| self.text()).collect(),
            properties: self.properties(depth),
        }
    }

    fn relationship(&mut self, depth: usize) -> Relationship {
        Relationship {
            id: RelationshipId(self.id()),
            relationship_type: self.text(),
            start: NodeId(self.id()),
            end: NodeId(self.id()),
            properties: self.properties(depth),
        }
    }

    fn value(&mut self, depth: usize) -> Value {
        let nested = depth < 4;
        match self.byte() % 15 {
            0 => Value::Null,
            1 => Value::Boolean(self.byte() % 2 == 1),
            2 => Value::Integer(i64::from(self.byte() as i8)),
            3 => Value::Integer(self.u64() as i64),
            4 => Value::Float(
                [0.0, -0.0, f64::NAN, f64::INFINITY, -f64::INFINITY, 1.5]
                    [usize::from(self.byte() % 6)],
            ),
            5 => Value::Float(f64::from_bits(self.u64())),
            6 => Value::String(self.text()),
            7 => Value::Bytes(self.text().into_bytes()),
            8 if nested => Value::List(
                (0..self.byte() % 5)
                    .map(|_| self.value(depth + 1))
                    .collect(),
            ),
            9 if nested => Value::Map(self.properties(depth)),
            10 if nested => Value::Node(self.node(depth)),
            11 if nested => Value::Relationship(self.relationship(depth)),
            12 if nested => {
                let hops = usize::from(self.byte() % 3);
                let nodes = (0..=hops).map(|_| self.node(depth)).collect();
                let relationships = (0..hops).map(|_| self.relationship(depth)).collect();
                Value::Path(Path::new(nodes, relationships))
            }
            13 => {
                let date = Date::from_ymd(
                    i64::from(self.byte() as i8) * 40,
                    1 + u32::from(self.byte() % 12),
                    1 + u32::from(self.byte() % 28),
                );
                date.map_or(Value::Null, |date| Value::Temporal(Temporal::Date(date)))
            }
            14 => {
                let duration = Duration::new(
                    i64::from(self.byte() as i8),
                    i64::from(self.byte() as i8),
                    self.u64() as i64,
                    i64::from(self.byte()),
                );
                duration.map_or(Value::Null, |duration| {
                    Value::Temporal(Temporal::Duration(duration))
                })
            }
            _ => {
                let mut value = Value::Integer(0);
                for _ in 0..self.byte() {
                    value = Value::List(vec![value]);
                }
                value
            }
        }
    }
}

/// Runs a fixed set of reading and writing queries over a small indexed graph with the two
/// parameters `$p` and `$q` built from `bytes`, which may hold any value, nodes and paths of IDs
/// that do not exist among them, then checks the database. Only a panic is a bug.
pub(crate) fn run_with_parameters(bytes: &[u8]) -> Result<(), String> {
    let fail = |error: crate::Error| error.to_string();
    let db = Database::in_memory().map_err(fail)?;
    db.create_keyword_index("words", IndexTarget::Label("A"), &["s"])
        .map_err(fail)?;
    db.create_vector_index(
        "vectors",
        IndexTarget::Label("A"),
        "v",
        2,
        VectorMetric::DotProduct,
    )
    .map_err(fail)?;
    db.create_property_index("by_i", IndexTarget::Label("A"), "i")
        .map_err(fail)?;
    db.execute(SETUP, &Parameters::new()).map_err(fail)?;
    let mut bytes = Bytes { bytes };
    let parameters = Parameters::new()
        .with("p", bytes.value(0))
        .with("q", bytes.value(0));
    for query in QUERIES {
        let _ = db.execute(query, &parameters);
    }
    db.check().map_err(fail)
}

#[cfg(test)]
#[path = "fuzz.test.rs"]
mod tests;
