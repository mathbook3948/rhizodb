use std::collections::BTreeMap;

use crate::error::Error;
use crate::error::ErrorKind;
use crate::graph::NodeId;
use crate::graph::RelationshipId;
use crate::result::Value;
use crate::temporal::Temporal;
use crate::value::MAX_DEPTH;
use crate::value::PropertyValue;

/// A value while a query runs. Graph elements are referred to by ID and read on demand.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Datum {
    Null,
    Boolean(bool),
    Integer(i64),
    Float(f64),
    String(String),
    Bytes(Vec<u8>),
    List(Vec<Datum>),
    Map(BTreeMap<String, Datum>),
    Node(NodeId),
    Relationship(RelationshipId),
    Path(PathIds),
    Temporal(Temporal),
}

/// A path by element IDs. `nodes` has one more entry than `relationships`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PathIds {
    pub(crate) nodes: Vec<NodeId>,
    pub(crate) relationships: Vec<RelationshipId>,
}

impl Datum {
    pub(crate) fn is_null(&self) -> bool {
        matches!(self, Datum::Null)
    }

    pub(crate) fn type_name(&self) -> &'static str {
        match self {
            Datum::Null => "null",
            Datum::Boolean(_) => "boolean",
            Datum::Integer(_) => "integer",
            Datum::Float(_) => "float",
            Datum::String(_) => "string",
            Datum::Bytes(_) => "bytes",
            Datum::List(_) => "list",
            Datum::Map(_) => "map",
            Datum::Node(_) => "node",
            Datum::Relationship(_) => "relationship",
            Datum::Path(_) => "path",
            Datum::Temporal(value) => value.type_name(),
        }
    }
}

impl From<PropertyValue> for Datum {
    fn from(value: PropertyValue) -> Self {
        match value {
            PropertyValue::Boolean(value) => Datum::Boolean(value),
            PropertyValue::Integer(value) => Datum::Integer(value),
            PropertyValue::Float(value) => Datum::Float(value),
            PropertyValue::String(value) => Datum::String(value),
            PropertyValue::Bytes(value) => Datum::Bytes(value),
            PropertyValue::List(items) => Datum::List(items.into_iter().map(Datum::from).collect()),
            PropertyValue::Temporal(value) => Datum::Temporal(value),
        }
    }
}

/// What writing `value` as a property does: `None` removes the property.
pub(crate) fn to_property(value: &Datum) -> Result<Option<PropertyValue>, Error> {
    match value {
        Datum::Null => Ok(None),
        other => to_stored(other, 0).map(Some),
    }
}

fn to_stored(value: &Datum, depth: usize) -> Result<PropertyValue, Error> {
    match value {
        Datum::Boolean(value) => Ok(PropertyValue::Boolean(*value)),
        Datum::Integer(value) => Ok(PropertyValue::Integer(*value)),
        Datum::Float(value) => Ok(PropertyValue::Float(*value)),
        Datum::String(value) => Ok(PropertyValue::String(value.clone())),
        Datum::Bytes(value) => Ok(PropertyValue::Bytes(value.clone())),
        Datum::Temporal(value) => Ok(PropertyValue::Temporal(value.clone())),
        Datum::List(items) => {
            if depth >= MAX_DEPTH {
                return Err(Error::new(
                    ErrorKind::InvalidPropertyType,
                    format!("a property list cannot nest deeper than {MAX_DEPTH} levels"),
                ));
            }
            items
                .iter()
                .map(|item| to_stored(item, depth + 1))
                .collect::<Result<_, _>>()
                .map(PropertyValue::List)
        }
        other => Err(Error::new(
            ErrorKind::InvalidPropertyType,
            format!("a {} cannot be stored as a property", other.type_name()),
        )),
    }
}

/// Turns a parameter into a runtime value. Graph elements are kept by ID.
pub(crate) fn from_parameter(value: &Value) -> Datum {
    match value {
        Value::Null => Datum::Null,
        Value::Boolean(value) => Datum::Boolean(*value),
        Value::Integer(value) => Datum::Integer(*value),
        Value::Float(value) => Datum::Float(*value),
        Value::String(value) => Datum::String(value.clone()),
        Value::Bytes(value) => Datum::Bytes(value.clone()),
        Value::Temporal(value) => Datum::Temporal(value.clone()),
        Value::List(items) => Datum::List(items.iter().map(from_parameter).collect()),
        Value::Map(entries) => Datum::Map(
            entries
                .iter()
                .map(|(key, value)| (key.clone(), from_parameter(value)))
                .collect(),
        ),
        Value::Node(node) => Datum::Node(node.id),
        Value::Relationship(relationship) => Datum::Relationship(relationship.id),
        Value::Path(path) => Datum::Path(PathIds {
            nodes: path.nodes().iter().map(|node| node.id).collect(),
            relationships: path
                .relationships()
                .iter()
                .map(|relationship| relationship.id)
                .collect(),
        }),
    }
}
