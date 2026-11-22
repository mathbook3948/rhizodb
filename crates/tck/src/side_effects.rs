use std::collections::BTreeSet;

use crate::value::Value;

/// A count of changes per kind of graph element.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Counts {
    /// Nodes.
    pub nodes: u64,
    /// Relationships.
    pub relationships: u64,
    /// Distinct labels.
    pub labels: u64,
    /// Properties, counted as entity, key, value triples.
    pub properties: u64,
}

/// What a query added to and removed from a graph.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct SideEffects {
    /// Things that exist after the query but not before.
    pub added: Counts,
    /// Things that existed before the query but not after.
    pub removed: Counts,
}

/// The node or relationship that owns a property.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Entity {
    /// A node, by engine-chosen id.
    Node(u64),
    /// A relationship, by engine-chosen id.
    Relationship(u64),
}

/// One property as an entity, key, value triple.
#[derive(Clone, Debug, PartialEq)]
pub struct PropertyEntry {
    /// Owner of the property.
    pub entity: Entity,
    /// Property key.
    pub key: String,
    /// Property value.
    pub value: Value,
}

/// The observable state of a graph, as far as side effects are concerned.
///
/// Ids only need to be stable within one graph, and the label set holds distinct label names.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GraphSnapshot {
    /// Ids of all nodes.
    pub nodes: Vec<u64>,
    /// Ids of all relationships.
    pub relationships: Vec<u64>,
    /// Distinct labels in use.
    pub labels: Vec<String>,
    /// All properties of all nodes and relationships.
    pub properties: Vec<PropertyEntry>,
}

impl GraphSnapshot {
    /// The side effects that turn `self` into `later`.
    pub fn diff(&self, later: &GraphSnapshot) -> SideEffects {
        let (nodes_added, nodes_removed) = set_difference(&self.nodes, &later.nodes);
        let (rels_added, rels_removed) = set_difference(&self.relationships, &later.relationships);
        let (labels_added, labels_removed) = set_difference(&self.labels, &later.labels);
        let (props_added, props_removed) = multiset_difference(&self.properties, &later.properties);
        SideEffects {
            added: Counts {
                nodes: nodes_added,
                relationships: rels_added,
                labels: labels_added,
                properties: props_added,
            },
            removed: Counts {
                nodes: nodes_removed,
                relationships: rels_removed,
                labels: labels_removed,
                properties: props_removed,
            },
        }
    }
}

fn set_difference<T: Ord>(before: &[T], after: &[T]) -> (u64, u64) {
    let before: BTreeSet<&T> = before.iter().collect();
    let after: BTreeSet<&T> = after.iter().collect();
    (
        after.difference(&before).count() as u64,
        before.difference(&after).count() as u64,
    )
}

fn multiset_difference(before: &[PropertyEntry], after: &[PropertyEntry]) -> (u64, u64) {
    (
        count_unmatched(after, before),
        count_unmatched(before, after),
    )
}

fn count_unmatched(items: &[PropertyEntry], against: &[PropertyEntry]) -> u64 {
    let mut remaining: Vec<&PropertyEntry> = against.iter().collect();
    let mut unmatched = 0;
    for item in items {
        match remaining.iter().position(|candidate| *candidate == item) {
            Some(index) => {
                remaining.swap_remove(index);
            }
            None => unmatched += 1,
        }
    }
    unmatched
}

#[cfg(test)]
#[path = "side_effects.test.rs"]
mod tests;
