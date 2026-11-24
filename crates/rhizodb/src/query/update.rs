use std::cell::RefCell;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

use crate::error::Error;
use crate::error::ErrorKind;
use crate::gql::ast::Direction;
use crate::gql::ast::LabelExpr;
use crate::graph::Direction as Adjacency;
use crate::graph::GraphTx;
use crate::graph::Label;
use crate::graph::NodeId;
use crate::graph::NodeRecord;
use crate::graph::Properties;
use crate::graph::PropertyKey;
use crate::graph::RelationshipId;
use crate::graph::RelationshipRecord;
use crate::graph::RelationshipType;
use crate::graph::Token;
use crate::storage::WriteTransaction;
use crate::temporal::Clocks;

use super::datum::Datum;
use super::datum::PathIds;
use super::datum::to_property;
use super::eval::Env;
use super::eval::Row;
use super::eval::set;
use super::eval::type_error;
use super::plan::Expr;
use super::plan::MergeClause;
use super::plan::NodePattern;
use super::plan::PatternPart;
use super::plan::RemoveAction;
use super::plan::SetAction;
use super::plan::UpdateClause;
use super::regex::Patterns;

/// What a query deleted. The records are kept so the query can still return them, and the
/// deleted nodes are checked for relationships left behind once the query has run.
#[derive(Default)]
pub(crate) struct Graveyard {
    pub(crate) deleted_nodes: BTreeSet<NodeId>,
    pub(crate) nodes: BTreeMap<NodeId, NodeRecord>,
    pub(crate) relationships: BTreeMap<RelationshipId, RelationshipRecord>,
}

/// Applies updating clauses to the rows of one query.
pub(crate) struct Writer<'a, W> {
    pub(crate) tx: &'a mut GraphTx<W>,
    pub(crate) slots: usize,
    pub(crate) parameters: &'a [Datum],
    pub(crate) now: &'a Clocks,
    pub(crate) patterns: &'a Patterns,
    pub(crate) graveyard: &'a mut Graveyard,
}

impl<W: WriteTransaction> Writer<'_, W> {
    /// Applies one updating clause to every row, in row order. Later rows see earlier rows' writes.
    pub(crate) fn apply(
        &mut self,
        clause: &UpdateClause,
        rows: Vec<Row>,
    ) -> Result<Vec<Row>, Error> {
        match clause {
            UpdateClause::Create(parts) => {
                let mut rows = rows;
                for row in &mut rows {
                    for part in parts {
                        self.create(part, row, false)?;
                    }
                }
                Ok(rows)
            }
            UpdateClause::Merge(merge) => {
                let mut out = Vec::new();
                for row in rows {
                    self.merge(merge, row, &mut out)?;
                }
                Ok(out)
            }
            UpdateClause::Set(actions) => {
                for row in &rows {
                    self.set_all(actions, row)?;
                }
                Ok(rows)
            }
            UpdateClause::Remove(actions) => {
                for row in &rows {
                    for action in actions {
                        self.remove(action, row)?;
                    }
                }
                Ok(rows)
            }
            UpdateClause::Delete {
                detach,
                targets,
                retain,
            } => {
                self.delete(*detach, *retain, targets, &rows)?;
                Ok(rows)
            }
        }
    }

    fn env(&self) -> Env<'_, W> {
        Env {
            tx: &*self.tx,
            slots: self.slots,
            parameters: self.parameters,
            now: self.now,
            graveyard: &*self.graveyard,
            patterns: self.patterns,
            check: None,
            columns: RefCell::default(),
            keys: RefCell::default(),
            labels: RefCell::default(),
            label_tokens: RefCell::default(),
        }
    }

    fn eval(&self, expr: &Expr, row: &Row) -> Result<Datum, Error> {
        self.env().eval(expr, row)
    }

    fn properties(
        &mut self,
        expr: Option<&Expr>,
        row: &Row,
        merging: bool,
    ) -> Result<Properties, Error> {
        let mut properties = Properties::new();
        let Some(expr) = expr else {
            return Ok(properties);
        };
        for (key, value) in self.map_operand(self.eval(expr, row)?)? {
            match to_property(&value)? {
                Some(value) => {
                    properties.insert(self.tx.intern_token::<PropertyKey>(&key)?, value);
                }
                None if merging => {
                    return Err(Error::new(
                        ErrorKind::NullMergeProperty,
                        format!("MERGE cannot use a null value for property `{key}`"),
                    ));
                }
                None => {}
            }
        }
        Ok(properties)
    }

    fn map_operand(&self, value: Datum) -> Result<BTreeMap<String, Datum>, Error> {
        let env = self.env();
        match value {
            Datum::Map(entries) => Ok(entries),
            Datum::Node(id) => env.property_map(&env.node(id)?.properties),
            Datum::Relationship(id) => env.property_map(&env.relationship(id)?.properties),
            Datum::Null => Ok(BTreeMap::new()),
            other => Err(type_error(format!(
                "expected a map of properties, got a {}",
                other.type_name()
            ))),
        }
    }

    fn node(
        &mut self,
        pattern: &NodePattern,
        row: &mut Row,
        merging: bool,
    ) -> Result<NodeId, Error> {
        if pattern.bound {
            return match row.get(pattern.slot) {
                Some(Datum::Node(id)) => Ok(*id),
                Some(other) => Err(type_error(format!(
                    "expected a node to connect, got a {}",
                    other.type_name()
                ))),
                None => Err(type_error("expected a node to connect")),
            };
        }
        let mut record = NodeRecord {
            properties: self.properties(pattern.properties.as_ref(), row, merging)?,
            ..NodeRecord::default()
        };
        for label in pattern.labels.iter().flat_map(LabelExpr::names) {
            record
                .labels
                .insert(self.tx.intern_token::<Label>(&label.0)?);
        }
        let id = self.tx.create_node(record)?;
        set(row, pattern.slot, Datum::Node(id));
        Ok(id)
    }

    fn create(&mut self, part: &PatternPart, row: &mut Row, merging: bool) -> Result<(), Error> {
        let mut current = self.node(&part.start, row, merging)?;
        let mut trail = PathIds {
            nodes: vec![current],
            relationships: Vec::new(),
        };
        for step in &part.steps {
            let next = self.node(&step.node, row, merging)?;
            let relationship = &step.relationship;
            let (source, target) = match relationship.direction {
                Direction::Incoming | Direction::IncomingOrUndirected => (next, current),
                Direction::Outgoing
                | Direction::UndirectedOrOutgoing
                | Direction::IncomingOrOutgoing
                | Direction::Any
                | Direction::Undirected => (current, next),
            };
            let rel_type = match &relationship.labels {
                Some(LabelExpr::Name(name)) => self.tx.intern_token::<RelationshipType>(&name.0)?,
                _ => return Err(type_error("a relationship to create needs one type")),
            };
            let record = RelationshipRecord {
                rel_type,
                source,
                target,
                properties: self.properties(relationship.properties.as_ref(), row, merging)?,
            };
            let id = self.tx.create_relationship(&record)?;
            set(row, relationship.slot, Datum::Relationship(id));
            trail.relationships.push(id);
            trail.nodes.push(next);
            current = next;
        }
        if let Some(path) = part.path {
            set(row, path, Datum::Path(trail));
        }
        Ok(())
    }

    fn merge(&mut self, merge: &MergeClause, row: Row, out: &mut Vec<Row>) -> Result<(), Error> {
        let mut matches = Vec::new();
        self.env()
            .match_patterns(std::slice::from_ref(&merge.pattern), &row, &mut |found| {
                matches.push(found.clone());
                Ok(true)
            })?;
        if matches.is_empty() {
            let mut row = row;
            self.create(&merge.pattern, &mut row, true)?;
            self.set_all(&merge.on_create, &row)?;
            out.push(row);
            return Ok(());
        }
        for found in matches {
            self.set_all(&merge.on_match, &found)?;
            out.push(found);
        }
        Ok(())
    }

    fn set_all(&mut self, actions: &[SetAction], row: &Row) -> Result<(), Error> {
        for action in actions {
            self.set(action, row)?;
        }
        Ok(())
    }

    fn set(&mut self, action: &SetAction, row: &Row) -> Result<(), Error> {
        match action {
            SetAction::Property { target, key, value } => {
                let target = self.eval(target, row)?;
                let value = to_property(&self.eval(value, row)?)?;
                self.write_property(&target, key, value)
            }
            SetAction::Properties {
                target,
                value,
                replace,
            } => {
                let target = row.get(*target).cloned().unwrap_or(Datum::Null);
                if target.is_null() {
                    return Ok(());
                }
                let entries = self.map_operand(self.eval(value, row)?)?;
                let mut changes = Vec::new();
                if *replace {
                    for key in self.map_operand(target.clone())?.into_keys() {
                        if !entries.contains_key(&key) {
                            changes.push((key, None));
                        }
                    }
                }
                for (key, value) in entries {
                    changes.push((key, to_property(&value)?));
                }
                self.write_properties(&target, changes)
            }
            SetAction::Labels { target, labels } => match row.get(*target) {
                Some(Datum::Node(id)) => {
                    for label in labels {
                        let token = self.tx.intern_token::<Label>(label)?;
                        self.tx.add_label(*id, token)?;
                    }
                    Ok(())
                }
                Some(Datum::Null) | None => Ok(()),
                Some(other) => Err(type_error(format!(
                    "labels can only be set on a node, not a {}",
                    other.type_name()
                ))),
            },
        }
    }

    fn property_token(
        &mut self,
        key: &str,
        writing: bool,
    ) -> Result<Option<Token<PropertyKey>>, Error> {
        Ok(if writing {
            Some(self.tx.intern_token(key)?)
        } else {
            self.tx.lookup_token(key)?
        })
    }

    fn write_property(
        &mut self,
        target: &Datum,
        key: &str,
        value: Option<crate::value::PropertyValue>,
    ) -> Result<(), Error> {
        match target {
            Datum::Null => Ok(()),
            Datum::Node(id) => {
                let Some(token) = self.property_token(key, value.is_some())? else {
                    return Ok(());
                };
                match value {
                    Some(value) => self.tx.set_node_property(*id, token, value)?,
                    None => self.tx.remove_node_property(*id, token)?,
                }
                Ok(())
            }
            Datum::Relationship(id) => {
                let Some(token) = self.property_token(key, value.is_some())? else {
                    return Ok(());
                };
                match value {
                    Some(value) => self.tx.set_relationship_property(*id, token, value)?,
                    None => self.tx.remove_relationship_property(*id, token)?,
                }
                Ok(())
            }
            other => Err(type_error(format!(
                "properties can only be set on a node or relationship, not a {}",
                other.type_name()
            ))),
        }
    }

    fn write_properties(
        &mut self,
        target: &Datum,
        changes: Vec<(String, Option<crate::value::PropertyValue>)>,
    ) -> Result<(), Error> {
        let Datum::Relationship(id) = target else {
            for (key, value) in changes {
                self.write_property(target, &key, value)?;
            }
            return Ok(());
        };
        let mut tokens = Vec::new();
        for (key, value) in changes {
            if let Some(token) = self.property_token(&key, value.is_some())? {
                tokens.push((token, value));
            }
        }
        if !tokens.is_empty() {
            self.tx.set_relationship_properties(*id, tokens)?;
        }
        Ok(())
    }

    fn remove(&mut self, action: &RemoveAction, row: &Row) -> Result<(), Error> {
        match action {
            RemoveAction::Property { target, key } => {
                let target = self.eval(target, row)?;
                self.write_property(&target, key, None)
            }
            RemoveAction::Labels { target, labels } => match row.get(*target) {
                Some(Datum::Node(id)) => {
                    for label in labels {
                        if let Some(token) = self.tx.lookup_token::<Label>(label)? {
                            self.tx.remove_label(*id, token)?;
                        }
                    }
                    Ok(())
                }
                Some(Datum::Null) | None => Ok(()),
                Some(other) => Err(type_error(format!(
                    "labels can only be removed from a node, not a {}",
                    other.type_name()
                ))),
            },
        }
    }

    fn delete(
        &mut self,
        detach: bool,
        retain: bool,
        targets: &[Expr],
        rows: &[Row],
    ) -> Result<(), Error> {
        let mut nodes: Vec<NodeId> = Vec::new();
        let mut relationships: Vec<RelationshipId> = Vec::new();
        for row in rows {
            for target in targets {
                collect_targets(self.eval(target, row)?, &mut nodes, &mut relationships)?;
            }
        }
        if detach {
            for node in &nodes {
                if self.tx.node_label_set(*node)?.is_some() {
                    for adjacent in self.tx.adjacent(*node, Adjacency::Both, None)? {
                        relationships.push(adjacent.relationship);
                    }
                }
            }
        }
        for relationship in relationships {
            if let Some(record) = self.tx.delete_relationship(relationship)?
                && retain
            {
                self.graveyard.relationships.insert(relationship, record);
            }
        }
        for node in nodes {
            if let Some(record) = self.tx.delete_node(node)? {
                self.graveyard.deleted_nodes.insert(node);
                if retain {
                    self.graveyard.nodes.insert(node, record);
                }
            }
        }
        Ok(())
    }
}

fn collect_targets(
    value: Datum,
    nodes: &mut Vec<NodeId>,
    relationships: &mut Vec<RelationshipId>,
) -> Result<(), Error> {
    match value {
        Datum::Null => {}
        Datum::Node(id) => nodes.push(id),
        Datum::Relationship(id) => relationships.push(id),
        Datum::Path(path) => {
            nodes.extend(path.nodes);
            relationships.extend(path.relationships);
        }
        Datum::List(items) => {
            for item in items {
                collect_targets(item, nodes, relationships)?;
            }
        }
        other => {
            return Err(type_error(format!(
                "DELETE takes a node, relationship, or path, not a {}",
                other.type_name()
            )));
        }
    }
    Ok(())
}
