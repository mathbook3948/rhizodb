use std::collections::BTreeMap;
use std::collections::btree_map;
use std::slice;
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
    Map(Arc<BTreeMap<Arc<str>, Datum>>),
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

const SHARED_KEYS: usize = 16;

/// How large one value that a query builds may grow: list items and map entries, and bytes of
/// strings and byte strings, each counted through every nested list and map.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Limits {
    pub(crate) items: usize,
    pub(crate) bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            items: 10_000_000,
            bytes: 1 << 28,
        }
    }
}

/// How much of [`Limits`] a value takes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Size {
    pub(crate) items: usize,
    pub(crate) bytes: usize,
}

impl Limits {
    /// Fails unless a value can hold `items` list items and map entries.
    pub(crate) fn check_items(self, items: usize) -> Result<(), Error> {
        if items > self.items {
            return Err(Error::new(
                ErrorKind::NumberOutOfRange,
                format!(
                    "a query cannot build a value of more than {} list items and map entries",
                    self.items
                ),
            ));
        }
        Ok(())
    }

    /// Fails unless a value can hold `bytes` bytes of strings and byte strings.
    pub(crate) fn check_bytes(self, bytes: usize) -> Result<(), Error> {
        if bytes > self.bytes {
            return Err(Error::new(
                ErrorKind::NumberOutOfRange,
                format!(
                    "a query cannot build a value of more than {} bytes of strings",
                    self.bytes
                ),
            ));
        }
        Ok(())
    }

    /// `text` as a string, or an error when it is longer than these limits allow.
    pub(crate) fn text(self, text: String) -> Result<Datum, Error> {
        self.check_bytes(text.len())?;
        Ok(Datum::String(text))
    }

    /// Fails unless `a` and `b` together fit in one value.
    pub(crate) fn joined(self, a: Size, b: Size) -> Result<Size, Error> {
        let size = Size {
            items: a.items.saturating_add(b.items),
            bytes: a.bytes.saturating_add(b.bytes),
        };
        self.check_items(size.items)?;
        self.check_bytes(size.bytes)?;
        Ok(size)
    }

    /// The size of the list of `items`, or an error once it passes these limits.
    pub(crate) fn list_size(self, items: &[Datum]) -> Result<Size, Error> {
        let mut size = Size::default();
        for item in items {
            self.add_item(&mut size, item)?;
        }
        Ok(size)
    }

    /// Adds `item` as one more item to `size`, failing when the sum passes these limits or
    /// `item` would nest too deeply as an item of a list or a map.
    pub(crate) fn add_item(self, size: &mut Size, item: &Datum) -> Result<(), Error> {
        size.items = size.items.saturating_add(1);
        self.check_items(size.items)?;
        self.add(size, item, MAX_NESTING - 1)
    }

    fn add(self, size: &mut Size, value: &Datum, levels: usize) -> Result<(), Error> {
        let (items, bytes) = match value {
            Datum::String(text) => (0, text.len()),
            Datum::Bytes(bytes) => (0, bytes.len()),
            Datum::List(items) => (items.len(), 0),
            Datum::Map(entries) => (entries.len(), 0),
            _ => return Ok(()),
        };
        size.items = size.items.saturating_add(items);
        size.bytes = size.bytes.saturating_add(bytes);
        self.check_items(size.items)?;
        self.check_bytes(size.bytes)?;
        match value {
            Datum::List(items) => {
                let levels = levels.checked_sub(1).ok_or_else(nesting_too_deep)?;
                items
                    .iter()
                    .try_for_each(|item| self.add(size, item, levels))
            }
            Datum::Map(entries) => {
                let levels = levels.checked_sub(1).ok_or_else(nesting_too_deep)?;
                entries.iter().try_for_each(|(key, item)| {
                    size.bytes = size.bytes.saturating_add(key.len());
                    self.check_bytes(size.bytes)?;
                    self.add(size, item, levels)
                })
            }
            _ => Ok(()),
        }
    }

    /// The list of `items`, or an error when it would pass these limits or nest too deeply.
    pub(crate) fn list(self, items: Vec<Datum>) -> Result<Datum, Error> {
        self.list_size(&items)?;
        Ok(Datum::List(Arc::new(items)))
    }

    /// The map of `entries`, or an error when it would pass these limits or nest too deeply.
    pub(crate) fn map(self, entries: BTreeMap<Arc<str>, Datum>) -> Result<Datum, Error> {
        let mut size = Size::default();
        for (key, item) in &entries {
            size.bytes = size.bytes.saturating_add(key.len());
            self.add_item(&mut size, item)?;
        }
        Ok(Datum::Map(Arc::new(entries)))
    }
}

/// A list that grows one item at a time and fails as soon as it passes its [`Limits`].
pub(crate) struct ListBuilder {
    limits: Limits,
    size: Size,
    items: Vec<Datum>,
}

impl ListBuilder {
    pub(crate) fn new(limits: Limits) -> Self {
        Self {
            limits,
            size: Size::default(),
            items: Vec::new(),
        }
    }

    pub(crate) fn push(&mut self, item: Datum) -> Result<(), Error> {
        self.limits.add_item(&mut self.size, &item)?;
        self.items.push(item);
        Ok(())
    }

    pub(crate) fn finish(self) -> Datum {
        Datum::List(Arc::new(self.items))
    }
}

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

/// What writing `value` as a property does: `None` removes the property.
pub(crate) fn to_property(value: &Datum) -> Result<Option<PropertyValue>, Error> {
    match value {
        Datum::Null => Ok(None),
        other => to_stored(other, 0).map(Some),
    }
}

/// [`to_property`] of an owned value, which keeps a string's buffer.
pub(crate) fn into_property(value: Datum) -> Result<Option<PropertyValue>, Error> {
    match value {
        Datum::String(value) => Ok(Some(PropertyValue::String(value))),
        other => to_property(&other),
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
            let mut stored = Vec::with_capacity(items.len());
            for item in items.iter() {
                stored.push(to_stored(item, depth + 1)?);
            }
            Ok(PropertyValue::List(stored))
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
    let mut open: Vec<Children<'_>> = Vec::new();
    let mut next = Some(value);
    loop {
        if let Some(value) = next.take() {
            let children = match value {
                Value::List(items) => Children::List(items.iter()),
                Value::Map(entries) => Children::Map(entries.values()),
                _ => Children::None,
            };
            if !matches!(children, Children::None) {
                if open.len() == MAX_NESTING {
                    return Err(nesting_too_deep());
                }
                open.push(children);
            }
        }
        let Some(children) = open.last_mut() else {
            break;
        };
        match children.next() {
            Some(child) => next = Some(child),
            None => {
                open.pop();
            }
        }
    }
    Ok(converted(value, &mut Vec::new()))
}

enum Children<'a> {
    None,
    List(slice::Iter<'a, Value>),
    Map(btree_map::Values<'a, String, Value>),
}

impl<'a> Children<'a> {
    fn next(&mut self) -> Option<&'a Value> {
        match self {
            Children::None => None,
            Children::List(items) => items.next(),
            Children::Map(entries) => entries.next(),
        }
    }
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

fn converted(value: &Value, keys: &mut Vec<Arc<str>>) -> Datum {
    match value {
        Value::Null => Datum::Null,
        Value::Boolean(value) => Datum::Boolean(*value),
        Value::Integer(value) => Datum::Integer(*value),
        Value::Float(value) => Datum::Float(*value),
        Value::String(value) => Datum::String(value.clone()),
        Value::Bytes(value) => Datum::Bytes(value.clone()),
        Value::Temporal(value) => Datum::Temporal(value.clone()),
        Value::List(items) => Datum::List(Arc::new(
            items.iter().map(|item| converted(item, keys)).collect(),
        )),
        Value::Map(entries) => {
            let mut map = BTreeMap::new();
            for (key, value) in entries {
                map.insert(shared_key(keys, key), converted(value, keys));
            }
            Datum::Map(Arc::new(map))
        }
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

fn shared_key(keys: &mut Vec<Arc<str>>, key: &str) -> Arc<str> {
    if let Some(held) = keys.iter().find(|held| &***held == key) {
        return Arc::clone(held);
    }
    let key: Arc<str> = Arc::from(key);
    if keys.len() < SHARED_KEYS {
        keys.push(Arc::clone(&key));
    }
    key
}
