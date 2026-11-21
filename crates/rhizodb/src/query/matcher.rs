use crate::error::Error;
use crate::gql::ast::Direction;
use crate::graph;
use crate::graph::Adjacent;
use crate::graph::Label;
use crate::graph::NodeId;
use crate::graph::PropertyKey;
use crate::graph::RelationshipId;
use crate::graph::RelationshipType;
use crate::storage::ReadTransaction;

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

struct Search<'s, 'e, T> {
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

    fn properties_match(
        &self,
        properties: &graph::Properties,
        expected: Option<&Expr>,
        row: &Row,
    ) -> Result<bool, Error> {
        let Some(expected) = expected else {
            return Ok(true);
        };
        let entries = match self.eval(expected, row)? {
            Datum::Map(entries) => entries,
            other => {
                return Err(type_error(format!(
                    "pattern properties must be a map, not a {}",
                    other.type_name()
                )));
            }
        };
        for (key, value) in &entries {
            let stored = match self.tx.lookup_token::<PropertyKey>(key)? {
                Some(token) => properties.get(&token).cloned().map(Datum::from),
                None => None,
            };
            let stored = stored.unwrap_or(Datum::Null);
            if equals(&stored, value) != Some(true) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn node_matches(&self, id: NodeId, pattern: &NodePattern, row: &Row) -> Result<bool, Error> {
        let Some(record) = self.tx.node(id)? else {
            return Ok(false);
        };
        for label in &pattern.labels {
            match self.tx.lookup_token::<Label>(label)? {
                Some(token) if record.labels.contains(&token) => {}
                _ => return Ok(false),
            }
        }
        self.properties_match(&record.properties, pattern.properties.as_ref(), row)
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
        self.properties_match(&record.properties, pattern.properties.as_ref(), row)
    }

    fn start_candidates(&self, pattern: &NodePattern, row: &Row) -> Result<Vec<NodeId>, Error> {
        if pattern.bound {
            return Ok(match row.get(pattern.slot) {
                Some(Datum::Node(id)) => vec![*id],
                _ => Vec::new(),
            });
        }
        match pattern.labels.first() {
            Some(label) => match self.tx.lookup_token::<Label>(label)? {
                Some(token) => Ok(self.tx.node_ids_with_label(token)?),
                None => Ok(Vec::new()),
            },
            None => Ok(self.tx.node_ids()?),
        }
    }

    fn expand(&self, node: NodeId, pattern: &RelationshipPattern) -> Result<Vec<Adjacent>, Error> {
        let direction = match pattern.direction {
            Direction::Outgoing => graph::Direction::Outgoing,
            Direction::Incoming => graph::Direction::Incoming,
            Direction::Undirected => graph::Direction::Both,
        };
        if let [only] = pattern.types.as_slice() {
            return match self.tx.lookup_token::<RelationshipType>(only)? {
                Some(token) => Ok(self.tx.adjacent(node, direction, Some(token))?),
                None => Ok(Vec::new()),
            };
        }
        let all = self.tx.adjacent(node, direction, None)?;
        if pattern.types.is_empty() {
            return Ok(all);
        }
        let mut tokens = Vec::new();
        for name in &pattern.types {
            if let Some(token) = self.tx.lookup_token::<RelationshipType>(name)? {
                tokens.push(token);
            }
        }
        Ok(all
            .into_iter()
            .filter(|adjacent| tokens.contains(&adjacent.rel_type))
            .collect())
    }
}

impl<T: ReadTransaction> Search<'_, '_, T> {
    fn part(&mut self, index: usize, row: &mut Row, emit: &mut Emit<'_>) -> Result<bool, Error> {
        let patterns = self.patterns;
        let Some(part) = patterns.get(index) else {
            return emit(row);
        };
        for id in self.env.start_candidates(&part.start, row)? {
            if !self.env.node_matches(id, &part.start, row)? {
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
            let value = Datum::List(walked.iter().copied().map(Datum::Relationship).collect());
            let accepted =
                !step.relationship.bound || row.get(step.relationship.slot) == Some(&value);
            if accepted {
                set(row, step.relationship.slot, value);
                if !self.arrive(index, step_index, step, current, row, trail, emit)? {
                    return Ok(false);
                }
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
        if !self.env.node_matches(node, &step.node, row)? {
            return Ok(true);
        }
        self.step(index, step_index + 1, node, row, trail, emit)
    }
}
