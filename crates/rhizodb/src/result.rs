use std::collections::BTreeMap;
use std::collections::BTreeSet;

use crate::graph::NodeId;
use crate::graph::RelationshipId;
use crate::temporal::Temporal;

/// Named query parameters, referred to as `$name` in the query text.
///
/// ```
/// use rhizodb::Parameters;
/// use rhizodb::Value;
///
/// let mut parameters = Parameters::new().with("name", "Ada");
/// parameters.insert("born", 1815);
/// assert_eq!(parameters.get("born"), Some(&Value::Integer(1815)));
/// assert_eq!(parameters.len(), 2);
///
/// let tags = Parameters::from([("a", 1), ("b", 2)]);
/// assert_eq!(tags.iter().map(|(name, _)| name).collect::<Vec<_>>(), ["a", "b"]);
/// ```
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Parameters {
    values: BTreeMap<String, Value>,
}

impl Parameters {
    /// No parameters.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the parameter `name`, returning the value it replaced.
    pub fn insert(&mut self, name: impl Into<String>, value: impl Into<Value>) -> Option<Value> {
        self.values.insert(name.into(), value.into())
    }

    /// These parameters with `name` set to `value`.
    #[must_use]
    pub fn with(mut self, name: impl Into<String>, value: impl Into<Value>) -> Self {
        self.insert(name, value);
        self
    }

    /// The value of the parameter `name`.
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.values.get(name)
    }

    /// The parameters in name order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Value)> {
        self.values
            .iter()
            .map(|(name, value)| (name.as_str(), value))
    }

    /// How many parameters are set.
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Whether no parameter is set.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

impl<K: Into<String>, V: Into<Value>> FromIterator<(K, V)> for Parameters {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        let mut parameters = Self::new();
        parameters.extend(iter);
        parameters
    }
}

impl<K: Into<String>, V: Into<Value>> Extend<(K, V)> for Parameters {
    fn extend<I: IntoIterator<Item = (K, V)>>(&mut self, iter: I) {
        for (name, value) in iter {
            self.insert(name, value);
        }
    }
}

impl<K: Into<String>, V: Into<Value>, const N: usize> From<[(K, V); N]> for Parameters {
    fn from(entries: [(K, V); N]) -> Self {
        entries.into_iter().collect()
    }
}

/// A value passed into or returned from a query.
///
/// A node, relationship, or path passed as a parameter refers to the stored element by its ID.
///
/// New variants may be added in any release, so a match needs a catch-all arm.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
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

impl From<bool> for Value {
    fn from(value: bool) -> Self {
        Self::Boolean(value)
    }
}

impl From<i64> for Value {
    fn from(value: i64) -> Self {
        Self::Integer(value)
    }
}

impl From<f64> for Value {
    fn from(value: f64) -> Self {
        Self::Float(value)
    }
}

impl From<String> for Value {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}

impl From<&str> for Value {
    fn from(value: &str) -> Self {
        Self::String(value.to_owned())
    }
}

/// A node as it was when the query finished.
#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    pub(crate) id: NodeId,
    pub(crate) labels: BTreeSet<String>,
    pub(crate) properties: BTreeMap<String, Value>,
}

impl Node {
    /// The node's ID.
    pub fn id(&self) -> NodeId {
        self.id
    }

    /// The node's labels.
    pub fn labels(&self) -> &BTreeSet<String> {
        &self.labels
    }

    /// Whether the node carries `label`.
    pub fn has_label(&self, label: &str) -> bool {
        self.labels.contains(label)
    }

    /// The node's properties.
    pub fn properties(&self) -> &BTreeMap<String, Value> {
        &self.properties
    }

    /// The value of the property `key`, or `None` when the node has no such property.
    pub fn property(&self, key: &str) -> Option<&Value> {
        self.properties.get(key)
    }
}

/// A relationship as it was when the query finished.
#[derive(Clone, Debug, PartialEq)]
pub struct Relationship {
    pub(crate) id: RelationshipId,
    pub(crate) relationship_type: String,
    pub(crate) start: NodeId,
    pub(crate) end: NodeId,
    pub(crate) properties: BTreeMap<String, Value>,
}

impl Relationship {
    /// The relationship's ID.
    pub fn id(&self) -> RelationshipId {
        self.id
    }

    /// The relationship's type.
    pub fn relationship_type(&self) -> &str {
        &self.relationship_type
    }

    /// The node the relationship leaves.
    pub fn start(&self) -> NodeId {
        self.start
    }

    /// The node the relationship enters.
    pub fn end(&self) -> NodeId {
        self.end
    }

    /// The relationship's properties.
    pub fn properties(&self) -> &BTreeMap<String, Value> {
        &self.properties
    }

    /// The value of the property `key`, or `None` when the relationship has no such property.
    pub fn property(&self, key: &str) -> Option<&Value> {
        self.properties.get(key)
    }
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
    pub(crate) fn into_parts(self) -> (Vec<Node>, Vec<Relationship>) {
        (self.nodes, self.relationships)
    }

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
