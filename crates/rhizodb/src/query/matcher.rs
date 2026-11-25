use std::convert::Infallible;
use std::sync::Arc;

use crate::error::Error;
use crate::gql::ast::Direction;
use crate::gql::ast::LabelExpr;
use crate::graph;
use crate::graph::Adjacent;
use crate::graph::NodeId;
use crate::graph::PropertyKey;
use crate::graph::RelationshipId;
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
use super::plan::MatchClause;
use super::plan::NodePattern;
use super::plan::PatternPart;
use super::plan::PatternQuery;
use super::plan::RelationshipPattern;
use super::plan::Slot;

/// Receives each complete match. Returning `false` stops the search.
pub(crate) type Emit<'a> = dyn FnMut(&Row) -> Result<bool, Error> + 'a;

type ExpectedProperties = Vec<(Option<Token<PropertyKey>>, Datum)>;

/// How a hop of one relationship pattern reads the relationships at a node.
#[derive(Clone, Copy, Debug)]
pub(crate) enum HopRead {
    /// The relationships on one side, of one type or of every type.
    Stored(graph::Direction, Option<Token<graph::RelationshipType>>),
    /// The relationships on one side, each tested against the pattern's type names.
    Tested(graph::Direction),
    /// None, since the pattern is undirected or names a type no relationship has.
    Nothing,
}

#[derive(Clone, Copy, Debug)]
struct Known {
    labels: bool,
    served: Option<Token<PropertyKey>>,
}

impl Known {
    const NOTHING: Self = Self {
        labels: false,
        served: None,
    };
}

struct Search<'s, 'e, T: ReadTransaction> {
    env: &'s Env<'e, T>,
    patterns: &'s [PatternPart],
    used: Vec<RelationshipId>,
    trails: Vec<PathIds>,
}

#[derive(Debug)]
enum Frame {
    /// The start nodes of a pattern part not tried yet.
    Part {
        index: usize,
        candidates: std::vec::IntoIter<NodeId>,
        known: Known,
    },
    /// The relationships of a single-hop step not tried yet. `extended` is set while the trail
    /// ends with the one tried last.
    Hop {
        index: usize,
        step_index: usize,
        adjacents: std::vec::IntoIter<Adjacent>,
        extended: bool,
    },
    /// One node of a variable-length walk, which began where the trail had `base` relationships.
    Walk {
        index: usize,
        step_index: usize,
        hops: Hops,
        base: usize,
        node: NodeId,
        state: WalkState,
    },
}

#[derive(Debug)]
enum WalkState {
    /// The walk ending at the node is yet to be matched against the rest of the pattern.
    Arriving,
    /// The walk ending at the node was matched, and the walks through it are yet to be tried.
    Arrived,
    /// The relationships that extend the walk not tried yet.
    Expanding {
        adjacents: std::vec::IntoIter<Adjacent>,
        extended: bool,
    },
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
            trails: vec![
                PathIds {
                    nodes: Vec::new(),
                    relationships: Vec::new(),
                };
                patterns.len()
            ],
        };
        let mut row = row.clone();
        search.run(&mut row, emit)?;
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
        Arc::unwrap_or_clone(entries)
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

    fn node_labelled(&self, id: NodeId, labels: &LabelExpr) -> Result<Option<bool>, Error> {
        match labels {
            LabelExpr::Name(name) => {
                let label = self.label_token(&name.0)?;
                self.with_labels(id, |found| {
                    label.is_some_and(|label| found.binary_search(&label).is_ok())
                })
            }
            labels => self
                .with_labels(id, |found| self.node_has_labels(found, labels))?
                .transpose(),
        }
    }

    fn node_matches(
        &self,
        id: NodeId,
        pattern: &NodePattern,
        known: Known,
        row: &Row,
    ) -> Result<bool, Error> {
        if known.labels && pattern.properties.is_none() {
            return Ok(true);
        }
        if !known.labels {
            let labelled = match &pattern.labels {
                Some(labels) => self.node_labelled(id, labels)?,
                None => self.with_labels(id, |_| true)?,
            };
            if labelled != Some(true) {
                return Ok(false);
            }
        }
        let Some(expected) = &pattern.properties else {
            return Ok(true);
        };
        let mut expected = self.expected_properties(expected, row)?;
        if let Some(served) = known.served {
            expected.retain(|(key, _)| *key != Some(served));
        }
        holds(&expected, |key| {
            Ok(self.node_property(id, Some(key))?.flatten())
        })
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
    ) -> Result<(Vec<NodeId>, Known), Error> {
        let pattern = &part.start;
        if pattern.bound {
            return Ok((
                match row.get(pattern.slot) {
                    Some(Datum::Node(id)) => vec![*id],
                    _ => Vec::new(),
                },
                Known::NOTHING,
            ));
        }
        let labels_known = label_scan_decides(pattern);
        if let Some(sought) = self.seek_start(part, row)? {
            let known = Known {
                labels: labels_known && sought.labelled,
                served: sought.served,
            };
            return Ok((sought.nodes, known));
        }
        let nodes = match self.morsel_of(part) {
            Some(ids) => ids.to_vec(),
            None => self.scanned_nodes(pattern)?,
        };
        let known = Known {
            labels: labels_known,
            served: None,
        };
        Ok((nodes, known))
    }

    /// How many nodes [`Env::scanned_nodes`] gives, read from the node counts.
    pub(crate) fn scanned_count(&self, pattern: &NodePattern) -> Result<u64, Error> {
        Ok(
            match pattern.labels.as_ref().and_then(LabelExpr::required) {
                Some(label) => match self.label_token(&label.0)? {
                    Some(token) => self.tx.count_nodes_with_label(token)?,
                    None => 0,
                },
                None => self.tx.count_nodes()?,
            },
        )
    }

    /// The nodes a pattern starts from when no seek serves it: those of its required label, or
    /// every node, in ascending ID order.
    pub(crate) fn scanned_nodes(&self, pattern: &NodePattern) -> Result<Vec<NodeId>, Error> {
        Ok(
            match pattern.labels.as_ref().and_then(LabelExpr::required) {
                Some(label) => match self.label_token(&label.0)? {
                    Some(token) => self.tx.node_ids_with_label(token)?,
                    None => Vec::new(),
                },
                None => self.tx.node_ids()?,
            },
        )
    }

    /// Whether node `id` has what `pattern` asks for, its labels and properties read in full.
    pub(crate) fn node_fits(
        &self,
        id: NodeId,
        pattern: &NodePattern,
        row: &Row,
    ) -> Result<bool, Error> {
        self.node_matches(id, pattern, Known::NOTHING, row)
    }

    /// How a hop of `pattern` reads the relationships at a node.
    pub(crate) fn hop_read(&self, pattern: &RelationshipPattern) -> Result<HopRead, Error> {
        let direction = match pattern.direction {
            Direction::Outgoing | Direction::UndirectedOrOutgoing => graph::Direction::Outgoing,
            Direction::Incoming | Direction::IncomingOrUndirected => graph::Direction::Incoming,
            Direction::IncomingOrOutgoing | Direction::Any => graph::Direction::Both,
            Direction::Undirected => return Ok(HopRead::Nothing),
        };
        Ok(match &pattern.labels {
            None => HopRead::Stored(direction, None),
            Some(LabelExpr::Name(only)) => match self.rel_type_token(&only.0)? {
                Some(token) => HopRead::Stored(direction, Some(token)),
                None => HopRead::Nothing,
            },
            Some(_) => HopRead::Tested(direction),
        })
    }

    /// Appends the relationships at `node` that match `pattern`, read as `read` says, to `found`.
    pub(crate) fn hop_into(
        &self,
        node: NodeId,
        pattern: &RelationshipPattern,
        read: HopRead,
        found: &mut Vec<Adjacent>,
    ) -> Result<(), Error> {
        let direction = match read {
            HopRead::Stored(direction, rel_type) => {
                return Ok(self.tx.adjacent_into(
                    &mut self.adjacency.borrow_mut(),
                    node,
                    direction,
                    rel_type,
                    found,
                )?);
            }
            HopRead::Tested(direction) => direction,
            HopRead::Nothing => return Ok(()),
        };
        let Some(labels) = &pattern.labels else {
            return Ok(());
        };
        let mut tokens = Vec::new();
        for name in labels.names() {
            tokens.push((name.0.as_str(), self.rel_type_token(&name.0)?));
        }
        let adjacent =
            self.tx
                .adjacent_with(&mut self.adjacency.borrow_mut(), node, direction, None)?;
        for adjacent in adjacent {
            let Ok(matched) = labels.matches::<Infallible>(true, &mut |name| {
                Ok(tokens
                    .iter()
                    .any(|(known, token)| *known == name && *token == Some(adjacent.rel_type)))
            });
            if matched {
                found.push(adjacent);
            }
        }
        Ok(())
    }

    fn expand(&self, node: NodeId, pattern: &RelationshipPattern) -> Result<Vec<Adjacent>, Error> {
        let mut found = Vec::new();
        self.hop_into(node, pattern, self.hop_read(pattern)?, &mut found)?;
        Ok(found)
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

/// The nodes a match clause matches one each, with nothing else to test, as in `MATCH (n)` or
/// `MATCH (n:Label)`: the slot each match binds, and the label, if any.
pub(crate) fn counted_nodes(found: &MatchClause) -> Option<(Slot, Option<&str>)> {
    let [part] = found.patterns.as_slice() else {
        return None;
    };
    let start = &part.start;
    let simple = !found.optional
        && found.filter.is_none()
        && part.path.is_none()
        && part.steps.is_empty()
        && part.seeks.is_empty()
        && !start.bound
        && start.properties.is_none();
    match &start.labels {
        _ if !simple => None,
        None => Some((start.slot, None)),
        Some(LabelExpr::Name(name)) => Some((start.slot, Some(&name.0))),
        Some(_) => None,
    }
}

fn label_scan_decides(pattern: &NodePattern) -> bool {
    !pattern.bound && matches!(pattern.labels, Some(LabelExpr::Name(_)))
}

impl<T: ReadTransaction> Search<'_, '_, T> {
    fn run(&mut self, row: &mut Row, emit: &mut Emit<'_>) -> Result<bool, Error> {
        let mut frames = Vec::new();
        if !self.begin_part(0, row, &mut frames, emit)? {
            return Ok(false);
        }
        while let Some(frame) = frames.last_mut() {
            let more = match frame {
                Frame::Part {
                    index,
                    candidates,
                    known,
                } => {
                    let (index, known) = (*index, *known);
                    match candidates.next() {
                        Some(id) => self.start(index, id, known, row, &mut frames, emit)?,
                        None => {
                            frames.pop();
                            true
                        }
                    }
                }
                Frame::Hop {
                    index,
                    step_index,
                    adjacents,
                    extended,
                } => {
                    let (index, step_index) = (*index, *step_index);
                    if std::mem::take(extended) {
                        self.retract(index);
                    }
                    match self.next_hop(index, step_index, adjacents, row)? {
                        Some(adjacent) => {
                            *extended = true;
                            self.extend(index, &adjacent);
                            self.arrive(index, step_index, adjacent.other, row, &mut frames, emit)?
                        }
                        None => {
                            frames.pop();
                            true
                        }
                    }
                }
                Frame::Walk {
                    index,
                    step_index,
                    hops,
                    base,
                    node,
                    state,
                } => {
                    let (index, step_index, hops, base, node) =
                        (*index, *step_index, *hops, *base, *node);
                    let depth = self.trails[index].relationships.len() - base;
                    match state {
                        WalkState::Arriving => {
                            *state = WalkState::Arrived;
                            if (depth as u64) < hops.min {
                                true
                            } else {
                                self.walk_end(
                                    index,
                                    step_index,
                                    base,
                                    node,
                                    row,
                                    &mut frames,
                                    emit,
                                )?
                            }
                        }
                        WalkState::Arrived => {
                            if hops.max.is_some_and(|max| depth as u64 >= max) {
                                frames.pop();
                            } else {
                                let relationship =
                                    &self.patterns[index].steps[step_index].relationship;
                                *state = WalkState::Expanding {
                                    adjacents: self.env.expand(node, relationship)?.into_iter(),
                                    extended: false,
                                };
                            }
                            true
                        }
                        WalkState::Expanding {
                            adjacents,
                            extended,
                        } => {
                            if std::mem::take(extended) {
                                self.retract(index);
                            }
                            match self.next_walk(index, step_index, adjacents, row)? {
                                Some(adjacent) => {
                                    *extended = true;
                                    self.extend(index, &adjacent);
                                    frames.push(Frame::Walk {
                                        index,
                                        step_index,
                                        hops,
                                        base,
                                        node: adjacent.other,
                                        state: WalkState::Arriving,
                                    });
                                }
                                None => {
                                    frames.pop();
                                }
                            }
                            true
                        }
                    }
                }
            };
            if !more {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn begin_part(
        &mut self,
        index: usize,
        row: &mut Row,
        frames: &mut Vec<Frame>,
        emit: &mut Emit<'_>,
    ) -> Result<bool, Error> {
        let Some(part) = self.patterns.get(index) else {
            return emit(row);
        };
        let (candidates, known) = self.env.start_candidates(part, row)?;
        frames.push(Frame::Part {
            index,
            candidates: candidates.into_iter(),
            known,
        });
        Ok(true)
    }

    fn start(
        &mut self,
        index: usize,
        id: NodeId,
        known: Known,
        row: &mut Row,
        frames: &mut Vec<Frame>,
        emit: &mut Emit<'_>,
    ) -> Result<bool, Error> {
        let start = &self.patterns[index].start;
        if !self.env.node_matches(id, start, known, row)? {
            return Ok(true);
        }
        set(row, start.slot, Datum::Node(id));
        let trail = &mut self.trails[index];
        trail.nodes.clear();
        trail.nodes.push(id);
        trail.relationships.clear();
        self.begin_step(index, 0, id, row, frames, emit)
    }

    fn begin_step(
        &mut self,
        index: usize,
        step_index: usize,
        current: NodeId,
        row: &mut Row,
        frames: &mut Vec<Frame>,
        emit: &mut Emit<'_>,
    ) -> Result<bool, Error> {
        let part = &self.patterns[index];
        let Some(step) = part.steps.get(step_index) else {
            if let Some(path) = part.path {
                set(row, path, Datum::Path(self.trails[index].clone()));
            }
            return self.begin_part(index + 1, row, frames, emit);
        };
        frames.push(match step.relationship.length {
            None => Frame::Hop {
                index,
                step_index,
                adjacents: self.env.expand(current, &step.relationship)?.into_iter(),
                extended: false,
            },
            Some(hops) => Frame::Walk {
                index,
                step_index,
                hops,
                base: self.trails[index].relationships.len(),
                node: current,
                state: WalkState::Arriving,
            },
        });
        Ok(true)
    }

    fn next_hop(
        &self,
        index: usize,
        step_index: usize,
        adjacents: &mut std::vec::IntoIter<Adjacent>,
        row: &mut Row,
    ) -> Result<Option<Adjacent>, Error> {
        let relationship = &self.patterns[index].steps[step_index].relationship;
        for adjacent in adjacents {
            if self.used.contains(&adjacent.relationship) {
                continue;
            }
            if relationship.bound
                && row.get(relationship.slot) != Some(&Datum::Relationship(adjacent.relationship))
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
            return Ok(Some(adjacent));
        }
        Ok(None)
    }

    fn next_walk(
        &self,
        index: usize,
        step_index: usize,
        adjacents: &mut std::vec::IntoIter<Adjacent>,
        row: &Row,
    ) -> Result<Option<Adjacent>, Error> {
        let relationship = &self.patterns[index].steps[step_index].relationship;
        for adjacent in adjacents {
            if !self.used.contains(&adjacent.relationship)
                && self
                    .env
                    .relationship_matches(adjacent.relationship, relationship, row)?
            {
                return Ok(Some(adjacent));
            }
        }
        Ok(None)
    }

    fn extend(&mut self, index: usize, adjacent: &Adjacent) {
        self.used.push(adjacent.relationship);
        let trail = &mut self.trails[index];
        trail.relationships.push(adjacent.relationship);
        trail.nodes.push(adjacent.other);
    }

    fn retract(&mut self, index: usize) {
        self.used.pop();
        let trail = &mut self.trails[index];
        trail.relationships.pop();
        trail.nodes.pop();
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "ending a walk needs both the walk and the search state"
    )]
    fn walk_end(
        &mut self,
        index: usize,
        step_index: usize,
        base: usize,
        node: NodeId,
        row: &mut Row,
        frames: &mut Vec<Frame>,
        emit: &mut Emit<'_>,
    ) -> Result<bool, Error> {
        let relationship = &self.patterns[index].steps[step_index].relationship;
        if !relationship.anonymous {
            let walked = &self.trails[index].relationships[base..];
            let value = Datum::List(Arc::new(
                walked.iter().copied().map(Datum::Relationship).collect(),
            ));
            if relationship.bound && row.get(relationship.slot) != Some(&value) {
                return Ok(true);
            }
            set(row, relationship.slot, value);
        }
        self.arrive(index, step_index, node, row, frames, emit)
    }

    fn arrive(
        &mut self,
        index: usize,
        step_index: usize,
        node: NodeId,
        row: &mut Row,
        frames: &mut Vec<Frame>,
        emit: &mut Emit<'_>,
    ) -> Result<bool, Error> {
        let pattern = &self.patterns[index].steps[step_index].node;
        if pattern.bound {
            if row.get(pattern.slot) != Some(&Datum::Node(node)) {
                return Ok(true);
            }
        } else {
            set(row, pattern.slot, Datum::Node(node));
        }
        if !self.env.node_matches(node, pattern, Known::NOTHING, row)? {
            return Ok(true);
        }
        self.begin_step(index, step_index + 1, node, row, frames, emit)
    }
}
