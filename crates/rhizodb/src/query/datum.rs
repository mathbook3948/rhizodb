use std::collections::BTreeMap;
use std::sync::Arc;

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
    List(Arc<Vec<Datum>>),
    Map(Arc<BTreeMap<String, Datum>>),
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
            PropertyValue::List(items) => {
                Datum::List(Arc::new(items.into_iter().map(Datum::from).collect()))
            }
            PropertyValue::Temporal(value) => Datum::Temporal(value),
        }
    }
}

/// How deep lists and maps may nest in a value while a query runs. Every step that walks a value
/// recursively, from comparing and hashing to dropping, stays within a bounded stack this way.
pub(crate) const MAX_NESTING: usize = 128;

fn nesting_too_deep() -> Error {
    Error::new(
        ErrorKind::NestingTooDeep,
        format!("a value cannot nest lists and maps deeper than {MAX_NESTING} levels"),
    )
}

fn fits(value: &Datum, levels: usize) -> bool {
    match value {
        Datum::List(items) => levels > 0 && items.iter().all(|item| fits(item, levels - 1)),
        Datum::Map(entries) => levels > 0 && entries.values().all(|item| fits(item, levels - 1)),
        _ => true,
    }
}

/// Fails unless `item` can be an item of a list or a map entry.
pub(crate) fn check_item(item: &Datum) -> Result<(), Error> {
    if fits(item, MAX_NESTING - 1) {
        Ok(())
    } else {
        Err(nesting_too_deep())
    }
}

/// The list of `items`, or an error when it would nest too deeply.
pub(crate) fn list(items: Vec<Datum>) -> Result<Datum, Error> {
    items.iter().try_for_each(check_item)?;
    Ok(Datum::List(Arc::new(items)))
}

/// The map of `entries`, or an error when it would nest too deeply.
pub(crate) fn map(entries: BTreeMap<String, Datum>) -> Result<Datum, Error> {
    entries.values().try_for_each(check_item)?;
    Ok(Datum::Map(Arc::new(entries)))
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

/// Turns a parameter into a runtime value, or fails when it nests too deeply. Graph elements are
/// kept by ID.
pub(crate) fn from_parameter(value: &Value) -> Result<Datum, Error> {
    let mut pending = vec![(value, 0)];
    while let Some((value, depth)) = pending.pop() {
        let items: Box<dyn Iterator<Item = &Value>> = match value {
            Value::List(items) => Box::new(items.iter()),
            Value::Map(entries) => Box::new(entries.values()),
            _ => continue,
        };
        if depth == MAX_NESTING {
            return Err(nesting_too_deep());
        }
        pending.extend(items.map(|item| (item, depth + 1)));
    }
    Ok(converted(value))
}

/// Drops `values` one level at a time, so a value of any depth is dropped without recursing.
pub(crate) fn dismantle(mut values: Vec<Value>) {
    while let Some(value) = values.pop() {
        match value {
            Value::List(items) => values.extend(items),
            Value::Map(entries) => values.extend(entries.into_values()),
            Value::Node(node) => values.extend(node.properties.into_values()),
            Value::Relationship(relationship) => {
                values.extend(relationship.properties.into_values());
            }
            Value::Path(path) => {
                let (nodes, relationships) = path.into_parts();
                values.extend(nodes.into_iter().map(Value::Node));
                values.extend(relationships.into_iter().map(Value::Relationship));
            }
            _ => {}
        }
    }
}

fn converted(value: &Value) -> Datum {
    match value {
        Value::Null => Datum::Null,
        Value::Boolean(value) => Datum::Boolean(*value),
        Value::Integer(value) => Datum::Integer(*value),
        Value::Float(value) => Datum::Float(*value),
        Value::String(value) => Datum::String(value.clone()),
        Value::Bytes(value) => Datum::Bytes(value.clone()),
        Value::Temporal(value) => Datum::Temporal(value.clone()),
        Value::List(items) => Datum::List(Arc::new(items.iter().map(converted).collect())),
        Value::Map(entries) => Datum::Map(Arc::new(
            entries
                .iter()
                .map(|(key, value)| (key.clone(), converted(value)))
                .collect(),
        )),
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
