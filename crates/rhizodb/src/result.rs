use std::collections::BTreeMap;
use std::collections::BTreeSet;

use crate::graph::NodeId;
use crate::graph::RelationshipId;

/// Named query parameters, referred to as `$name` in the query text.
pub type Parameters = BTreeMap<String, Value>;

/// A value passed into or returned from a query.
///
/// A node, relationship, or path passed as a parameter refers to the stored element by its ID.
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
}

/// A node as it was when the query finished.
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
/// A query without `RETURN` has no columns and no rows.
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
