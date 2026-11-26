use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt;
use std::fmt::Write;

use crate::graph::NodeId;
use crate::graph::RelationshipId;
use crate::query::float_text;
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
/// An integer and a float are different values even when they are equal numbers. New variants
/// may be added in any release, so a match needs a catch-all arm.
///
/// # Display
///
/// A value displays as GQL text, so an output line reads like the query that could have produced
/// it. For `Null`, `Boolean`, `Integer`, a finite `Float`, `String`, `Temporal`, and lists and
/// maps of them, the text is an expression that evaluates back to an equal value, and that holds
/// in every later release. A datetime in a named zone reads back through the zone rules of the
/// build that reads it, so a changed rule can move it to another instant. `Bytes`, a non-finite
/// `Float`, and anything holding a node, relationship, or path display for people to read, and
/// their text may change.
///
/// | Value | Output |
/// | --- | --- |
/// | `Null`, `Boolean` | `null`, `true`, `false` |
/// | `Integer` | `42`, `-7` |
/// | `Float` | `1.5`, `1.0`, `NaN`, `Infinity`, `-Infinity`, `1.0E10`, as `toString` writes it |
/// | `String` | `'text'`, with `\\`, `\'`, `\n`, `\r`, `\t`, `\b`, `\f` and `\u00XX` for other control characters escaped |
/// | `Bytes` | `X'00ff'`, two lowercase hex digits per byte |
/// | `List` | `[1, 'a']`, `[]` |
/// | `Map` | `{k: 1, m: {}}` in key order, `{}`; a key that is not a plain name is in backticks |
/// | `Node`, `Relationship`, `Path` | as documented on those types |
/// | `Temporal` | a constructor call such as `date('2024-02-29')` or `duration('PT1H')` |
///
/// A float always has a fraction or an exponent, so it never reads as an integer, and a string is
/// always quoted, so it never reads as a temporal.
///
/// ```
/// use rhizodb::Database;
/// use rhizodb::Parameters;
/// use rhizodb::Value;
///
/// let db = Database::in_memory()?;
/// let result = db.execute(
///     "RETURN 1 AS i, 1.0 AS f, 'it\\'s' AS s, [null, true] AS l, {k: 'v'} AS m, date('2024-02-29') AS d",
///     &Parameters::new(),
/// )?;
/// let shown: Vec<String> = result.rows()[0].iter().map(Value::to_string).collect();
/// assert_eq!(shown, ["1", "1.0", r"'it\'s'", "[null, true]", "{k: 'v'}", "date('2024-02-29')"]);
/// assert_eq!(Value::Bytes(vec![0, 255]).to_string(), "X'00ff'");
/// # Ok::<(), rhizodb::Error>(())
/// ```
///
/// Values travel into a query as parameters and come back unchanged:
///
/// ```
/// use rhizodb::Database;
/// use rhizodb::Parameters;
/// use rhizodb::Value;
///
/// let db = Database::in_memory()?;
/// let parameters = Parameters::from([
///     ("tags", Value::List(vec![Value::String("a".to_owned())])),
///     ("raw", Value::Bytes(vec![0, 255])),
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
/// assert_eq!(Some(&row[4]), parameters.get("tags"));
/// assert_eq!(row[5], Value::Bytes(vec![0, 255]));
/// # Ok::<(), rhizodb::Error>(())
/// ```
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
///
/// A node displays as `(:Label1:Label2 {key: value})` in label and key order, without its ID, and
/// leaves out the labels or the properties it does not have, down to `()`. Two nodes that differ
/// only by ID display the same.
///
/// ```
/// use rhizodb::Database;
/// use rhizodb::Parameters;
/// use rhizodb::Value;
///
/// let db = Database::in_memory()?;
/// let result = db.execute("INSERT (n:Person:Admin {name: 'Ada'}) RETURN n", &Parameters::new())?;
/// let Value::Node(node) = &result.rows()[0][0] else { panic!("not a node") };
/// assert!(node.has_label("Admin"));
/// assert_eq!(node.property("name").unwrap(), &Value::String("Ada".to_owned()));
/// let found = db.execute(
///     "MATCH (n) WHERE id(n) = $id RETURN n.name",
///     &Parameters::from([("id".to_owned(), Value::Integer(node.id().as_u64() as i64))]),
/// )?;
/// assert_eq!(found.rows().len(), 1);
/// assert_eq!(node.to_string(), "(:Admin:Person {name: 'Ada'})");
/// # Ok::<(), rhizodb::Error>(())
/// ```
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
///
/// A relationship displays as `[:TYPE {key: value}]` in key order, without its ID or its end
/// nodes, and leaves out the properties it does not have.
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
/// assert_eq!(r.relationship_type(), "KNOWS");
/// assert_eq!(r.start(), a.id());
/// assert_eq!(r.property("since").unwrap(), &Value::Integer(2020));
/// assert_eq!(r.to_string(), "[:KNOWS {since: 2020}]");
/// # Ok::<(), rhizodb::Error>(())
/// ```
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
///
/// A path displays as its nodes and relationships between angle brackets, such as
/// `<(:A)-[:T]->(:B)<-[:T]-(:C)>`. A relationship that starts at the node before it points
/// forward, as `-[:T]->`, and one that ends there points back, as `<-[:T]-`.
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
/// assert_eq!(path.relationships()[1].start(), path.nodes()[2].id());
/// assert_eq!(path.to_string(), "<(:Stop {n: 1})-[:TO]->(:Stop {n: 2})<-[:TO]-(:Stop {n: 3})>");
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

impl fmt::Display for Value {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => formatter.write_str("null"),
            Self::Boolean(value) => write!(formatter, "{value}"),
            Self::Integer(value) => write!(formatter, "{value}"),
            Self::Float(value) => formatter.write_str(&float_text(*value)),
            Self::String(text) => write_string(formatter, text),
            Self::Bytes(bytes) => {
                formatter.write_str("X'")?;
                for byte in bytes {
                    write!(formatter, "{byte:02x}")?;
                }
                formatter.write_char('\'')
            }
            Self::List(items) => {
                formatter.write_char('[')?;
                for (at, item) in items.iter().enumerate() {
                    if at > 0 {
                        formatter.write_str(", ")?;
                    }
                    write!(formatter, "{item}")?;
                }
                formatter.write_char(']')
            }
            Self::Map(entries) => write_map(formatter, entries),
            Self::Node(node) => node.fmt(formatter),
            Self::Relationship(relationship) => relationship.fmt(formatter),
            Self::Path(path) => path.fmt(formatter),
            Self::Temporal(temporal) => {
                write!(formatter, "{}('{temporal}')", temporal.constructor())
            }
        }
    }
}

impl fmt::Display for Node {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_char('(')?;
        for label in &self.labels {
            formatter.write_char(':')?;
            write_name(formatter, label)?;
        }
        if !self.properties.is_empty() {
            if !self.labels.is_empty() {
                formatter.write_char(' ')?;
            }
            write_map(formatter, &self.properties)?;
        }
        formatter.write_char(')')
    }
}

impl fmt::Display for Relationship {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[:")?;
        write_name(formatter, &self.relationship_type)?;
        if !self.properties.is_empty() {
            formatter.write_char(' ')?;
            write_map(formatter, &self.properties)?;
        }
        formatter.write_char(']')
    }
}

impl fmt::Display for Path {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_char('<')?;
        if let Some((first, rest)) = self.nodes.split_first() {
            write!(formatter, "{first}")?;
            let mut previous = first.id;
            for (relationship, node) in self.relationships.iter().zip(rest) {
                if relationship.start == previous {
                    write!(formatter, "-{relationship}->")?;
                } else {
                    write!(formatter, "<-{relationship}-")?;
                }
                write!(formatter, "{node}")?;
                previous = node.id;
            }
        }
        formatter.write_char('>')
    }
}

fn write_map(formatter: &mut fmt::Formatter<'_>, entries: &BTreeMap<String, Value>) -> fmt::Result {
    formatter.write_char('{')?;
    for (at, (key, value)) in entries.iter().enumerate() {
        if at > 0 {
            formatter.write_str(", ")?;
        }
        write_name(formatter, key)?;
        write!(formatter, ": {value}")?;
    }
    formatter.write_char('}')
}

fn write_string(formatter: &mut fmt::Formatter<'_>, text: &str) -> fmt::Result {
    formatter.write_char('\'')?;
    for character in text.chars() {
        match character {
            '\\' => formatter.write_str("\\\\")?,
            '\'' => formatter.write_str("\\'")?,
            '\n' => formatter.write_str("\\n")?,
            '\r' => formatter.write_str("\\r")?,
            '\t' => formatter.write_str("\\t")?,
            '\u{8}' => formatter.write_str("\\b")?,
            '\u{c}' => formatter.write_str("\\f")?,
            control if control.is_control() => write!(formatter, "\\u{:04x}", u32::from(control))?,
            other => formatter.write_char(other)?,
        }
    }
    formatter.write_char('\'')
}

fn write_name(formatter: &mut fmt::Formatter<'_>, name: &str) -> fmt::Result {
    let mut characters = name.chars();
    let plain = characters
        .next()
        .is_some_and(|first| first.is_alphabetic() || first == '_')
        && characters.all(|character| character.is_alphanumeric() || character == '_');
    if plain {
        return formatter.write_str(name);
    }
    formatter.write_char('`')?;
    for character in name.chars() {
        if character == '`' {
            formatter.write_char('`')?;
        }
        formatter.write_char(character)?;
    }
    formatter.write_char('`')
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
