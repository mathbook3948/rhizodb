use std::collections::BTreeMap;
use std::collections::BTreeSet;

use crate::graph::NodeId;
use crate::graph::RelationshipId;
use crate::temporal::Temporal;

/// Named query parameters, referred to as `$name` in the query text.
///
/// ```
/// use rhizodb::Database;
/// use rhizodb::Parameters;
/// use rhizodb::Value;
///
/// let db = Database::in_memory()?;
/// let parameters = Parameters::from([("name".to_owned(), Value::String("Ada".to_owned()))]);
/// db.execute("INSERT (:Person {name: $name})", &parameters)?;
/// assert_eq!(db.execute("MATCH (p:Person {name: $name}) RETURN p", &parameters)?.rows().len(), 1);
/// # Ok::<(), rhizodb::Error>(())
/// ```
pub type Parameters = BTreeMap<String, Value>;

/// A value passed into or returned from a query.
///
/// A node, relationship, or path passed as a parameter refers to the stored element by its ID.
/// An integer and a float are different values even when they are equal numbers.
///
/// ```
/// use rhizodb::Database;
/// use rhizodb::Parameters;
/// use rhizodb::Value;
///
/// let db = Database::in_memory()?;
/// let parameters = Parameters::from([
///     ("tags".to_owned(), Value::List(vec![Value::String("a".to_owned())])),
///     ("raw".to_owned(), Value::Bytes(vec![0, 255])),
/// ]);
/// let result = db.execute(
///     "RETURN 1 AS i, 1.0 AS f, null AS n, {k: true} AS m, $tags AS tags, $raw AS raw",
///     &parameters,
/// )?;
/// let row = &result.rows()[0];
/// assert_eq!(row[0], Value::Integer(1));
/// assert_eq!(row[1], Value::Float(1.0));
/// assert_eq!(row[2], Value::Null);
/// assert_eq!(row[3], Value::Map([("k".to_owned(), Value::Boolean(true))].into()));
/// assert_eq!(row[4], parameters["tags"]);
/// assert_eq!(row[5], Value::Bytes(vec![0, 255]));
/// # Ok::<(), rhizodb::Error>(())
/// ```
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// The absence of a value.
    Null,
    /// A boolean.
    Boolean(bool),
    /// A 64-bit signed integer.
    Integer(i64),
    /// A 64-bit float.
    Float(f64),
    /// A string.
    String(String),
    /// A byte string.
    Bytes(Vec<u8>),
    /// A list.
    List(Vec<Value>),
    /// A map with string keys.
    Map(BTreeMap<String, Value>),
    /// A node with its labels and properties as they were when the query finished.
    Node(Node),
    /// A relationship with its type and properties as they were when the query finished.
    Relationship(Relationship),
    /// A path.
    Path(Path),
    /// A date, a time, a datetime, or a duration.
    Temporal(Temporal),
}

/// A node as it was when the query finished.
///
/// ```
/// use rhizodb::Database;
/// use rhizodb::Parameters;
/// use rhizodb::Value;
///
/// let db = Database::in_memory()?;
/// let result = db.execute("INSERT (n:Person:Admin {name: 'Ada'}) RETURN n", &Parameters::new())?;
/// let Value::Node(node) = &result.rows()[0][0] else { panic!("not a node") };
/// assert!(node.labels.contains("Admin"));
/// assert_eq!(node.properties["name"], Value::String("Ada".to_owned()));
/// let found = db.execute(
///     "MATCH (n) WHERE id(n) = $id RETURN n.name",
///     &Parameters::from([("id".to_owned(), Value::Integer(node.id.as_u64() as i64))]),
/// )?;
/// assert_eq!(found.rows().len(), 1);
/// # Ok::<(), rhizodb::Error>(())
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    /// The node's ID.
    pub id: NodeId,
    /// The node's labels.
    pub labels: BTreeSet<String>,
    /// The node's properties.
    pub properties: BTreeMap<String, Value>,
}

/// A relationship as it was when the query finished.
///
/// ```
/// use rhizodb::Database;
/// use rhizodb::Parameters;
/// use rhizodb::Value;
///
/// let db = Database::in_memory()?;
/// let result = db.execute(
///     "INSERT (a:A)-[r:KNOWS {since: 2020}]->(b:B) RETURN a, r",
///     &Parameters::new(),
/// )?;
/// let [Value::Node(a), Value::Relationship(r)] = result.rows()[0].as_slice() else { panic!() };
/// assert_eq!(r.rel_type, "KNOWS");
/// assert_eq!(r.start, a.id);
/// assert_eq!(r.properties["since"], Value::Integer(2020));
/// # Ok::<(), rhizodb::Error>(())
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct Relationship {
    /// The relationship's ID.
    pub id: RelationshipId,
    /// The relationship's type.
    pub rel_type: String,
    /// The node the relationship leaves.
    pub start: NodeId,
    /// The node the relationship enters.
    pub end: NodeId,
    /// The relationship's properties.
    pub properties: BTreeMap<String, Value>,
}

/// A walk through the graph: nodes alternating with the relationships between them.
///
/// There is always one more node than relationships, and relationship `i` connects node `i` and
/// node `i + 1` in either direction.
///
/// ```
/// use rhizodb::Database;
/// use rhizodb::Parameters;
/// use rhizodb::Value;
///
/// let db = Database::in_memory()?;
/// db.execute("INSERT (:Stop {n: 1})-[:TO]->(:Stop {n: 2})<-[:TO]-(:Stop {n: 3})", &Parameters::new())?;
/// let result = db.execute(
///     "MATCH p = (:Stop {n: 1})-[:TO]->()<-[:TO]-(:Stop {n: 3}) RETURN p",
///     &Parameters::new(),
/// )?;
/// let Value::Path(path) = &result.rows()[0][0] else { panic!("not a path") };
/// assert_eq!(path.nodes().len(), 3);
/// assert_eq!(path.relationships().len(), 2);
/// assert_eq!(path.relationships()[1].start, path.nodes()[2].id);
/// # Ok::<(), rhizodb::Error>(())
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct Path {
    nodes: Vec<Node>,
    relationships: Vec<Relationship>,
}

impl Path {
    pub(crate) fn new(nodes: Vec<Node>, relationships: Vec<Relationship>) -> Self {
        Self {
            nodes,
            relationships,
        }
    }

    pub(crate) fn into_parts(self) -> (Vec<Node>, Vec<Relationship>) {
        (self.nodes, self.relationships)
    }

    /// The nodes in walk order, starting with the start node.
    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }

    /// The relationships in walk order.
    pub fn relationships(&self) -> &[Relationship] {
        &self.relationships
    }
}

/// The table a query returns: column names and rows aligned with them.
///
/// A query without `RETURN` has no columns and no rows. Every row is held in memory, so a query
/// over a large graph uses `LIMIT`, or `SKIP` and `LIMIT` in pages, to bound what it returns.
///
/// ```
/// use rhizodb::Database;
/// use rhizodb::Parameters;
/// use rhizodb::Value;
///
/// let db = Database::in_memory()?;
/// let result = db.execute("UNWIND [1, 2] AS x RETURN x, x * 10 AS y", &Parameters::new())?;
/// assert_eq!(result.columns(), ["x", "y"]);
/// assert_eq!(result.rows()[1], [Value::Integer(2), Value::Integer(20)]);
/// assert!(db.execute("INSERT ()", &Parameters::new())?.rows().is_empty());
/// # Ok::<(), rhizodb::Error>(())
/// ```
#[derive(Clone, Debug, Default, PartialEq)]
pub struct QueryResult {
    columns: Vec<String>,
    rows: Vec<Vec<Value>>,
}

impl QueryResult {
    pub(crate) fn new(columns: Vec<String>, rows: Vec<Vec<Value>>) -> Self {
        Self { columns, rows }
    }

    /// Column names in projection order.
    pub fn columns(&self) -> &[String] {
        &self.columns
    }

    /// Rows, each with one value per column.
    pub fn rows(&self) -> &[Vec<Value>] {
        &self.rows
    }
}
