use std::convert::Infallible;

use crate::error::Error;
use crate::gql::ast::Direction;
use crate::gql::ast::LabelExpr;
use crate::graph;
use crate::graph::Adjacent;
use crate::graph::EncodedNode;
use crate::graph::Label;
use crate::graph::NodeId;
use crate::graph::PropertyKey;
use crate::graph::RelationshipId;
use crate::graph::RelationshipType;
use crate::graph::Token;
use crate::storage::ReadTransaction;
use crate::value::PropertyValue;

use super::compare::equals;
use super::datum::Datum;
use super::datum::PathIds;
use super::eval::Env;
use super::eval::Row;
use super::eval::set;
use super::eval::type_error;
use super::plan::Expr;
use super::plan::Hops;
use super::plan::NodePattern;
use super::plan::PatternPart;
use super::plan::PatternQuery;
use super::plan::RelationshipPattern;
use super::plan::Step;

/// Receives each complete match. Returning `false` stops the search.
pub(crate) type Emit<'a> = dyn FnMut(&Row) -> Result<bool, Error> + 'a;

type ExpectedProperties = Vec<(Option<Token<PropertyKey>>, Datum)>;

struct Search<'s, 'e, T: ReadTransaction> {
    env: &'s Env<'e, T>,
    patterns: &'s [PatternPart],
    used: Vec<RelationshipId>,
}

impl<T: ReadTransaction> Env<'_, T> {
    /// Calls `emit` with every extension of `row` that matches all `patterns`.
    pub(crate) fn match_patterns(
        &self,
        patterns: &[PatternPart],
        row: &Row,
        emit: &mut Emit<'_>,
    ) -> Result<(), Error> {
        let mut search = Search {
            env: self,
            patterns,
            used: Vec::new(),
        };
        let mut row = row.clone();
        search.part(0, &mut row, emit)?;
        Ok(())
    }

    /// Matches a pattern predicate or comprehension from `row`, applying its filter.
    pub(crate) fn match_query(
        &self,
        query: &PatternQuery,
        row: &Row,
        emit: &mut Emit<'_>,
    ) -> Result<(), Error> {
        self.match_patterns(&query.patterns, row, &mut |matched| match &query.filter {
            Some(filter) if !self.predicate(filter, matched)? => Ok(true),
            _ => emit(matched),
        })
    }

    fn expected_properties(&self, expected: &Expr, row: &Row) -> Result<ExpectedProperties, Error> {
        let entries = match self.eval(expected, row)? {
            Datum::Map(entries) => entries,
            other => {
                return Err(type_error(format!(
                    "pattern properties must be a map, not a {}",
                    other.type_name()
                )));
            }
        };
        entries
            .into_iter()
            .map(|(key, value)| Ok((self.tx.lookup_token::<PropertyKey>(&key)?, value)))
            .collect()
    }

    fn properties_match(
        &self,
        stored: impl Fn(Token<PropertyKey>) -> Result<Option<PropertyValue>, Error>,
        expected: Option<&Expr>,
        row: &Row,
    ) -> Result<bool, Error> {
        match expected {
            Some(expected) => holds(&self.expected_properties(expected, row)?, stored),
            None => Ok(true),
        }
    }

    fn node_labelled(&self, node: &EncodedNode<'_>, labels: &LabelExpr) -> Result<bool, Error> {
        match labels {
            LabelExpr::Name(name) => match self.tx.lookup_token::<Label>(&name.0)? {
                Some(label) => Ok(node.has_label(label)?),
                None => Ok(false),
            },
            labels => self.node_has_labels(&node.labels()?, labels),
        }
    }

    fn node_matches(
        &self,
        id: NodeId,
        pattern: &NodePattern,
        labels_known: bool,
        row: &Row,
    ) -> Result<bool, Error> {
        if labels_known && pattern.properties.is_none() {
            return Ok(true);
        }
        if !labels_known {
            let labelled = self.with_node(id, |node| match &pattern.labels {
                Some(labels) => self.node_labelled(&node, labels),
                None => Ok(true),
            })?;
            if labelled != Some(true) {
                return Ok(false);
            }
        }
        let Some(expected) = &pattern.properties else {
            return Ok(true);
        };
        let expected = self.expected_properties(expected, row)?;
        let matches = self.with_node(id, |node| holds(&expected, |key| Ok(node.property(key)?)))?;
        Ok(matches == Some(true))
    }

    fn relationship_matches(
        &self,
        id: RelationshipId,
        pattern: &RelationshipPattern,
        row: &Row,
    ) -> Result<bool, Error> {
        if pattern.properties.is_none() {
            return Ok(true);
        }
        let Some(record) = self.tx.relationship(id)? else {
            return Ok(false);
        };
        self.properties_match(
            |key| Ok(record.properties.get(&key).cloned()),
            pattern.properties.as_ref(),
            row,
        )
    }

    fn start_candidates(
        &self,
        part: &PatternPart,
        row: &Row,
    ) -> Result<(Vec<NodeId>, bool), Error> {
        let pattern = &part.start;
        if pattern.bound {
            return Ok((
                match row.get(pattern.slot) {
                    Some(Datum::Node(id)) => vec![*id],
                    _ => Vec::new(),
                },
                false,
            ));
        }
        let labels_known = label_scan_decides(pattern);
        if let Some(sought) = self.seek_start(part, row)? {
            return Ok((sought.nodes, labels_known && sought.labelled));
        }
        let nodes = match pattern.labels.as_ref().and_then(LabelExpr::required) {
            Some(label) => match self.tx.lookup_token::<Label>(&label.0)? {
                Some(token) => self.tx.node_ids_with_label(token)?,
                None => Vec::new(),
            },
            None => self.tx.node_ids()?,
        };
        Ok((nodes, labels_known))
    }

    fn expand(&self, node: NodeId, pattern: &RelationshipPattern) -> Result<Vec<Adjacent>, Error> {
        let direction = match pattern.direction {
            Direction::Outgoing | Direction::UndirectedOrOutgoing => graph::Direction::Outgoing,
            Direction::Incoming | Direction::IncomingOrUndirected => graph::Direction::Incoming,
            Direction::IncomingOrOutgoing | Direction::Any => graph::Direction::Both,
            Direction::Undirected => return Ok(Vec::new()),
        };
        let labels = match &pattern.labels {
            None => return Ok(self.tx.adjacent(node, direction, None)?),
            Some(LabelExpr::Name(only)) => {
                return match self.tx.lookup_token::<RelationshipType>(&only.0)? {
                    Some(token) => Ok(self.tx.adjacent(node, direction, Some(token))?),
                    None => Ok(Vec::new()),
                };
            }
            Some(labels) => labels,
        };
        let mut tokens = Vec::new();
        for name in labels.names() {
            tokens.push((
                name.0.as_str(),
                self.tx.lookup_token::<RelationshipType>(&name.0)?,
            ));
        }
        let mut kept = Vec::new();
        for adjacent in self.tx.adjacent(node, direction, None)? {
            let Ok(found) = labels.matches::<Infallible>(true, &mut |name| {
                Ok(tokens
                    .iter()
                    .any(|(known, token)| *known == name && *token == Some(adjacent.rel_type)))
            });
            if found {
                kept.push(adjacent);
            }
        }
        Ok(kept)
    }
}

fn holds(
    expected: &ExpectedProperties,
    stored: impl Fn(Token<PropertyKey>) -> Result<Option<PropertyValue>, Error>,
) -> Result<bool, Error> {
    for (key, value) in expected {
        let stored = match key {
            Some(key) => stored(*key)?.map_or(Datum::Null, Datum::from),
            None => Datum::Null,
        };
        if equals(&stored, value) != Some(true) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn label_scan_decides(pattern: &NodePattern) -> bool {
    !pattern.bound && matches!(pattern.labels, Some(LabelExpr::Name(_)))
}

impl<T: ReadTransaction> Search<'_, '_, T> {
    fn part(&mut self, index: usize, row: &mut Row, emit: &mut Emit<'_>) -> Result<bool, Error> {
        let patterns = self.patterns;
        let Some(part) = patterns.get(index) else {
            return emit(row);
        };
        let (candidates, labels_known) = self.env.start_candidates(part, row)?;
        for id in candidates {
            if !self.env.node_matches(id, &part.start, labels_known, row)? {
                continue;
            }
            set(row, part.start.slot, Datum::Node(id));
            let mut trail = PathIds {
                nodes: vec![id],
                relationships: Vec::new(),
            };
            if !self.step(index, 0, id, row, &mut trail, emit)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn step(
        &mut self,
        index: usize,
        step_index: usize,
        current: NodeId,
        row: &mut Row,
        trail: &mut PathIds,
        emit: &mut Emit<'_>,
    ) -> Result<bool, Error> {
        let patterns = self.patterns;
        let part = &patterns[index];
        let Some(step) = part.steps.get(step_index) else {
            if let Some(path) = part.path {
                set(row, path, Datum::Path(trail.clone()));
            }
            return self.part(index + 1, row, emit);
        };
        match step.relationship.length {
            None => {
                for adjacent in self.env.expand(current, &step.relationship)? {
                    if self.used.contains(&adjacent.relationship) {
                        continue;
                    }
                    let relationship = &step.relationship;
                    if relationship.bound
                        && row.get(relationship.slot)
                            != Some(&Datum::Relationship(adjacent.relationship))
                    {
                        continue;
                    }
                    if !self
                        .env
                        .relationship_matches(adjacent.relationship, relationship, row)?
                    {
                        continue;
                    }
                    set(
                        row,
                        relationship.slot,
                        Datum::Relationship(adjacent.relationship),
                    );
                    self.used.push(adjacent.relationship);
                    trail.relationships.push(adjacent.relationship);
                    trail.nodes.push(adjacent.other);
                    let more =
                        self.arrive(index, step_index, step, adjacent.other, row, trail, emit);
                    trail.nodes.pop();
                    trail.relationships.pop();
                    self.used.pop();
                    if !more? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            Some(hops) => {
                let mut walked = Vec::new();
                self.hops(
                    index,
                    step_index,
                    step,
                    hops,
                    current,
                    &mut walked,
                    row,
                    trail,
                    emit,
                )
            }
        }
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the search threads its whole state through recursion"
    )]
    fn hops(
        &mut self,
        index: usize,
        step_index: usize,
        step: &Step,
        hops: Hops,
        current: NodeId,
        walked: &mut Vec<RelationshipId>,
        row: &mut Row,
        trail: &mut PathIds,
        emit: &mut Emit<'_>,
    ) -> Result<bool, Error> {
        let depth = walked.len() as u64;
        if depth >= hops.min {
            let relationship = &step.relationship;
            let accepted = if relationship.anonymous {
                true
            } else {
                let value = Datum::List(walked.iter().copied().map(Datum::Relationship).collect());
                let accepted = !relationship.bound || row.get(relationship.slot) == Some(&value);
                if accepted {
                    set(row, relationship.slot, value);
                }
                accepted
            };
            if accepted && !self.arrive(index, step_index, step, current, row, trail, emit)? {
                return Ok(false);
            }
        }
        if hops.max.is_some_and(|max| depth >= max) {
            return Ok(true);
        }
        for adjacent in self.env.expand(current, &step.relationship)? {
            if self.used.contains(&adjacent.relationship)
                || !self
                    .env
                    .relationship_matches(adjacent.relationship, &step.relationship, row)?
            {
                continue;
            }
            self.used.push(adjacent.relationship);
            walked.push(adjacent.relationship);
            trail.relationships.push(adjacent.relationship);
            trail.nodes.push(adjacent.other);
            let more = self.hops(
                index,
                step_index,
                step,
                hops,
                adjacent.other,
                walked,
                row,
                trail,
                emit,
            );
            trail.nodes.pop();
            trail.relationships.pop();
            walked.pop();
            self.used.pop();
            if !more? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the search threads its whole state through recursion"
    )]
    fn arrive(
        &mut self,
        index: usize,
        step_index: usize,
        step: &Step,
        node: NodeId,
        row: &mut Row,
        trail: &mut PathIds,
        emit: &mut Emit<'_>,
    ) -> Result<bool, Error> {
        if step.node.bound {
            if row.get(step.node.slot) != Some(&Datum::Node(node)) {
                return Ok(true);
            }
        } else {
            set(row, step.node.slot, Datum::Node(node));
        }
        if !self.env.node_matches(node, &step.node, false, row)? {
            return Ok(true);
        }
        self.step(index, step_index + 1, node, row, trail, emit)
    }
}
