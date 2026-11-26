use std::collections::BTreeMap;
use std::collections::btree_map;
use std::fmt;
use std::ops::Deref;
use std::ops::DerefMut;
use std::slice;
use std::sync::Arc;
use std::sync::OnceLock;

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
    List(Arc<Measured<Vec<Datum>>>),
    Map(Arc<Measured<BTreeMap<Arc<str>, Datum>>>),
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

/// A list or map with its [`Measure`], worked out the first time a query asks for it and kept
/// until the list or map changes, so that measuring a value never walks a part of it that was
/// measured before.
#[derive(Clone, Default)]
pub(crate) struct Measured<T> {
    value: T,
    measure: OnceLock<Measure>,
}

impl<T> Measured<T> {
    pub(crate) fn new(value: T) -> Self {
        Self {
            value,
            measure: OnceLock::new(),
        }
    }

    pub(crate) fn into_inner(self) -> T {
        self.value
    }

    fn known(&self) -> Option<Measure> {
        self.measure.get().copied()
    }

    fn remember(&mut self, measure: Measure) {
        self.measure = OnceLock::from(measure);
    }
}

impl<T> From<T> for Measured<T> {
    fn from(value: T) -> Self {
        Self::new(value)
    }
}

impl<T> Deref for Measured<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.value
    }
}

impl<T> DerefMut for Measured<T> {
    fn deref_mut(&mut self) -> &mut T {
        self.measure.take();
        &mut self.value
    }
}

impl<T: PartialEq> PartialEq for Measured<T> {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

impl<T: fmt::Debug> fmt::Debug for Measured<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.value.fmt(f)
    }
}

/// The list or map that `shared` holds, copied only when something else still holds it.
pub(crate) fn owned<T: Clone>(shared: Arc<Measured<T>>) -> T {
    Arc::unwrap_or_clone(shared).into_inner()
}

impl<T: IntoIterator> IntoIterator for Measured<T> {
    type Item = T::Item;
    type IntoIter = T::IntoIter;

    fn into_iter(self) -> Self::IntoIter {
        self.value.into_iter()
    }
}

impl<'a, T> IntoIterator for &'a Measured<T>
where
    &'a T: IntoIterator,
{
    type Item = <&'a T as IntoIterator>::Item;
    type IntoIter = <&'a T as IntoIterator>::IntoIter;

    fn into_iter(self) -> Self::IntoIter {
        (&self.value).into_iter()
    }
}

/// The size of a value and how many levels of lists and maps it nests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Measure {
    size: Size,
    depth: usize,
}

impl Datum {
    /// The list of `items`, not yet measured.
    pub(crate) fn list(items: Vec<Datum>) -> Self {
        Datum::List(Arc::new(Measured::new(items)))
    }

    /// The map of `entries`, not yet measured.
    pub(crate) fn map(entries: BTreeMap<Arc<str>, Datum>) -> Self {
        Datum::Map(Arc::new(Measured::new(entries)))
    }

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
            PropertyValue::List(items) => Datum::list(items.into_iter().map(Datum::from).collect()),
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
        let size = a.plus(b);
        self.check(size)?;
        Ok(size)
    }

    /// Fails unless a value of `size` fits these limits.
    pub(crate) fn check(self, size: Size) -> Result<(), Error> {
        self.check_items(size.items)?;
        self.check_bytes(size.bytes)
    }

    /// Fails unless `value` fits these limits and nests no deeper than a value may.
    pub(crate) fn check_value(self, value: &Datum) -> Result<(), Error> {
        self.check(measure(value, MAX_NESTING)?.size)
    }

    /// The list of `items`, or an error when it would pass these limits or nest too deeply.
    pub(crate) fn list(self, items: Vec<Datum>) -> Result<Datum, Error> {
        let value = Datum::list(items);
        self.check_value(&value)?;
        Ok(value)
    }

    /// The map of `entries`, or an error when it would pass these limits or nest too deeply.
    pub(crate) fn map(self, entries: BTreeMap<Arc<str>, Datum>) -> Result<Datum, Error> {
        let value = Datum::map(entries);
        self.check_value(&value)?;
        Ok(value)
    }

    /// Appends `item` to `list`, failing before it changes `list` when the result would pass
    /// these limits or nest too deeply. Once `list` is measured, this takes time in the size of
    /// `item` alone, beside the copy of `list` that a shared `list` needs.
    pub(crate) fn push(
        self,
        list: &mut Arc<Measured<Vec<Datum>>>,
        item: Datum,
    ) -> Result<(), Error> {
        let grown = self.with_item(list, &item)?;
        let list = Arc::make_mut(list);
        list.push(item);
        list.remember(grown);
        Ok(())
    }

    /// Puts `item` first in `list`, failing before it changes `list` when the result would pass
    /// these limits or nest too deeply.
    pub(crate) fn prepend(
        self,
        item: Datum,
        list: &mut Arc<Measured<Vec<Datum>>>,
    ) -> Result<(), Error> {
        let grown = self.with_item(list, &item)?;
        let list = Arc::make_mut(list);
        list.insert(0, item);
        list.remember(grown);
        Ok(())
    }

    /// Appends the items of `tail` to `list`, failing before it changes `list` when the result
    /// would pass these limits. Once both are measured, this takes time in the length of `tail`
    /// alone, beside the copy of `list` that a shared `list` needs.
    pub(crate) fn extend(
        self,
        list: &mut Arc<Measured<Vec<Datum>>>,
        tail: Arc<Measured<Vec<Datum>>>,
    ) -> Result<(), Error> {
        let mut grown = measure_list(list, MAX_NESTING)?;
        let added = measure_list(&tail, MAX_NESTING)?;
        grown.size = self.joined(grown.size, added.size)?;
        grown.depth = grown.depth.max(added.depth);
        let list = Arc::make_mut(list);
        match Arc::try_unwrap(tail) {
            Ok(tail) => list.extend(tail.into_inner()),
            Err(tail) => list.extend(tail.iter().cloned()),
        }
        list.remember(grown);
        Ok(())
    }

    fn with_item(self, list: &Measured<Vec<Datum>>, item: &Datum) -> Result<Measure, Error> {
        let mut grown = measure_list(list, MAX_NESTING)?;
        let added = measure(item, MAX_NESTING - 1)?;
        grown.size = self.joined(grown.size, item_size(added))?;
        grown.depth = grown.depth.max(added.depth + 1);
        Ok(grown)
    }
}

fn item_size(measure: Measure) -> Size {
    Size {
        items: measure.size.items.saturating_add(1),
        bytes: measure.size.bytes,
    }
}

fn measure(value: &Datum, levels: usize) -> Result<Measure, Error> {
    let measure = match value {
        Datum::String(text) => Measure {
            size: Size {
                items: 0,
                bytes: text.len(),
            },
            depth: 0,
        },
        Datum::Bytes(bytes) => Measure {
            size: Size {
                items: 0,
                bytes: bytes.len(),
            },
            depth: 0,
        },
        Datum::List(items) => measure_list(items, levels)?,
        Datum::Map(entries) => measure_map(entries, levels)?,
        _ => Measure::default(),
    };
    if measure.depth > levels {
        return Err(nesting_too_deep());
    }
    Ok(measure)
}

fn measure_list(items: &Measured<Vec<Datum>>, levels: usize) -> Result<Measure, Error> {
    if let Some(known) = items.known() {
        return Ok(known);
    }
    let children = levels.checked_sub(1).ok_or_else(nesting_too_deep)?;
    let mut total = Measure {
        size: Size::default(),
        depth: 1,
    };
    for item in items.iter() {
        let child = measure(item, children)?;
        total.add(item_size(child), child.depth);
    }
    let _ = items.measure.set(total);
    Ok(total)
}

fn measure_map(
    entries: &Measured<BTreeMap<Arc<str>, Datum>>,
    levels: usize,
) -> Result<Measure, Error> {
    if let Some(known) = entries.known() {
        return Ok(known);
    }
    let children = levels.checked_sub(1).ok_or_else(nesting_too_deep)?;
    let mut total = Measure {
        size: Size::default(),
        depth: 1,
    };
    for (key, item) in entries.iter() {
        let child = measure(item, children)?;
        let mut size = item_size(child);
        size.bytes = size.bytes.saturating_add(key.len());
        total.add(size, child.depth);
    }
    let _ = entries.measure.set(total);
    Ok(total)
}

impl Measure {
    fn add(&mut self, size: Size, child_depth: usize) {
        self.size = self.size.plus(size);
        self.depth = self.depth.max(child_depth.saturating_add(1));
    }
}

/// A list that grows one item at a time and fails as soon as it passes its [`Limits`].
pub(crate) struct ListBuilder {
    limits: Limits,
    measure: Measure,
    items: Vec<Datum>,
}

impl ListBuilder {
    pub(crate) fn new(limits: Limits) -> Self {
        Self {
            limits,
            measure: Measure {
                size: Size::default(),
                depth: 1,
            },
            items: Vec::new(),
        }
    }

    pub(crate) fn push(&mut self, item: Datum) -> Result<(), Error> {
        let added = measure(&item, MAX_NESTING - 1)?;
        self.limits
            .check(self.measure.size.plus(item_size(added)))?;
        self.measure.add(item_size(added), added.depth);
        self.items.push(item);
        Ok(())
    }

    /// Appends the items of `other`, failing before it changes this list when the result would
    /// pass its limits. Both lists are measured already, so this takes no time per item.
    pub(crate) fn append(&mut self, mut other: ListBuilder) -> Result<(), Error> {
        self.measure.size = self.limits.joined(self.measure.size, other.measure.size)?;
        self.measure.depth = self.measure.depth.max(other.measure.depth);
        if self.items.is_empty() {
            self.items = other.items;
        } else {
            self.items.append(&mut other.items);
        }
        Ok(())
    }

    /// The items so far, in the order they were pushed.
    pub(crate) fn into_items(self) -> Vec<Datum> {
        self.items
    }

    pub(crate) fn finish(self) -> Datum {
        let mut list = Measured::new(self.items);
        list.remember(self.measure);
        Datum::List(Arc::new(list))
    }
}

impl Size {
    fn plus(self, other: Size) -> Size {
        Size {
            items: self.items.saturating_add(other.items),
            bytes: self.bytes.saturating_add(other.bytes),
        }
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
        Value::List(items) => Datum::list(items.iter().map(|item| converted(item, keys)).collect()),
        Value::Map(entries) => {
            let mut map = BTreeMap::new();
            for (key, value) in entries {
                map.insert(shared_key(keys, key), converted(value, keys));
            }
            Datum::map(map)
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
