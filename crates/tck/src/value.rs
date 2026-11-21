use std::collections::BTreeMap;
use std::collections::BTreeSet;

/// Property map of a node, relationship, or map value.
pub type Properties = BTreeMap<String, Value>;

/// How the elements of lists are compared.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Lists {
    /// Lists are equal when their elements are equal pairwise.
    #[default]
    Strict,
    /// Lists are equal when they hold the same elements in any order.
    IgnoreElementOrder,
}

/// A node as the TCK sees it. Identity is the label set plus the properties.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Node {
    /// Labels of the node.
    pub labels: BTreeSet<String>,
    /// Properties of the node.
    pub properties: Properties,
}

/// A relationship as the TCK sees it. Identity is the type plus the properties.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Relationship {
    /// Relationship type.
    pub rel_type: String,
    /// Properties of the relationship.
    pub properties: Properties,
}

/// Direction in which a path traverses a relationship.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Direction {
    /// Traversed from the previous node to the next node.
    Forward,
    /// Traversed from the next node to the previous node.
    Backward,
}

/// One hop of a path.
#[derive(Clone, Debug, PartialEq)]
pub struct PathStep {
    /// Relationship that is traversed.
    pub relationship: Relationship,
    /// Direction of the traversal.
    pub direction: Direction,
    /// Node reached by the hop.
    pub node: Node,
}

/// A path: a start node followed by hops.
#[derive(Clone, Debug, PartialEq)]
pub struct Path {
    /// First node of the path.
    pub start: Node,
    /// Hops after the start node.
    pub steps: Vec<PathStep>,
}

/// A Cypher value in the TCK literal syntax.
#[derive(Clone, Debug)]
pub enum Value {
    /// The null value.
    Null,
    /// A boolean.
    Boolean(bool),
    /// A 64-bit integer.
    Integer(i64),
    /// A 64-bit float. NaN equals NaN and negative zero equals zero.
    Float(f64),
    /// A string.
    String(String),
    /// A list.
    List(Vec<Value>),
    /// A map with string keys.
    Map(Properties),
    /// A node.
    Node(Node),
    /// A relationship.
    Relationship(Relationship),
    /// A path.
    Path(Path),
}

impl PartialEq for Value {
    fn eq(&self, other: &Value) -> bool {
        self.equals(other, Lists::Strict)
    }
}

impl Value {
    /// Compares two values with the TCK equality rules and the given list mode.
    pub fn equals(&self, other: &Value, lists: Lists) -> bool {
        match (self, other) {
            (Value::Null, Value::Null) => true,
            (Value::Boolean(a), Value::Boolean(b)) => a == b,
            (Value::Integer(a), Value::Integer(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => a == b || (a.is_nan() && b.is_nan()),
            (Value::String(a), Value::String(b)) => a == b,
            (Value::List(a), Value::List(b)) => lists_equal(a, b, lists),
            (Value::Map(a), Value::Map(b)) => properties_equal(a, b, lists),
            (Value::Node(a), Value::Node(b)) => nodes_equal(a, b, lists),
            (Value::Relationship(a), Value::Relationship(b)) => relationships_equal(a, b, lists),
            (Value::Path(a), Value::Path(b)) => {
                nodes_equal(&a.start, &b.start, lists)
                    && a.steps.len() == b.steps.len()
                    && a.steps.iter().zip(&b.steps).all(|(x, y)| {
                        x.direction == y.direction
                            && relationships_equal(&x.relationship, &y.relationship, lists)
                            && nodes_equal(&x.node, &y.node, lists)
                    })
            }
            _ => false,
        }
    }
}

fn lists_equal(a: &[Value], b: &[Value], lists: Lists) -> bool {
    if a.len() != b.len() {
        return false;
    }
    match lists {
        Lists::Strict => a.iter().zip(b).all(|(x, y)| x.equals(y, lists)),
        Lists::IgnoreElementOrder => {
            let mut unmatched: Vec<&Value> = b.iter().collect();
            a.iter().all(
                |x| match unmatched.iter().position(|y| x.equals(y, lists)) {
                    Some(index) => {
                        unmatched.swap_remove(index);
                        true
                    }
                    None => false,
                },
            )
        }
    }
}

fn properties_equal(a: &Properties, b: &Properties, lists: Lists) -> bool {
    a.len() == b.len()
        && a.iter()
            .all(|(key, x)| b.get(key).is_some_and(|y| x.equals(y, lists)))
}

fn nodes_equal(a: &Node, b: &Node, lists: Lists) -> bool {
    a.labels == b.labels && properties_equal(&a.properties, &b.properties, lists)
}

fn relationships_equal(a: &Relationship, b: &Relationship, lists: Lists) -> bool {
    a.rel_type == b.rel_type && properties_equal(&a.properties, &b.properties, lists)
}

#[cfg(test)]
#[path = "value.test.rs"]
mod tests;
