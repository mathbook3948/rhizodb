use std::cell::RefCell;
use std::convert::Infallible;
use std::sync::Arc;

use crate::error::Error;
use crate::gql::ast::Direction;
use crate::gql::ast::LabelExpr;
use crate::graph;
use crate::graph::Adjacent;
use crate::graph::Label;
use crate::graph::NodeId;
use crate::graph::PropertyKey;
use crate::graph::RelationshipId;
use crate::graph::RelationshipRecord;
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
use super::seek::Sought;

/// Receives each complete match. Returning `false` stops the search.
pub(crate) type Emit<'a> = dyn FnMut(&Row) -> Result<bool, Error> + 'a;

type ExpectedProperties = Vec<(Option<Token<PropertyKey>>, Datum)>;

#[derive(Default)]
struct Fixed<'s>(RefCell<Vec<(&'s Expr, ExpectedProperties)>>);

fn row_free(expr: &Expr) -> bool {
    match expr {
        Expr::Constant(_) | Expr::Parameter(_) => true,
        Expr::List(items) => items.iter().all(row_free),
        Expr::Map(entries) => entries.iter().all(|(_, value)| row_free(value)),
        _ => false,
    }
}

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
    trails: Vec<Trail<'s>>,
    fixed: Fixed<'s>,
}

struct Trail<'s> {
    part: &'s PatternPart,
    nodes: Vec<NodeId>,
    relationships: Vec<RelationshipId>,
    split: usize,
}

#[derive(Debug)]
enum Frame {
    /// The start nodes of a pattern part not tried yet.
    Part {
        index: usize,
        candidates: Candidates,
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

/// The start nodes of a pattern part: those a lookup, a bound node, or a morsel gives, or the
/// nodes of the start's label, or every node, read one row group at a time.
#[derive(Debug)]
pub(crate) enum Candidates {
    Listed(std::vec::IntoIter<NodeId>),
    Scanned {
        label: Option<Token<Label>>,
        group: std::vec::IntoIter<NodeId>,
        next: Option<u64>,
    },
}

impl Candidates {
    fn next<T: ReadTransaction>(&mut self, env: &Env<'_, T>) -> Result<Option<NodeId>, Error> {
        match self {
            Self::Listed(ids) => Ok(ids.next()),
            Self::Scanned { label, group, next } => loop {
                if let Some(id) = group.next() {
                    return Ok(Some(id));
                }
                let Some(from) = *next else {
                    return Ok(None);
                };
                match env.tx.node_group(from, *label)? {
                    Some((at, ids)) => {
                        *group = ids.into_iter();
                        *next = at.checked_add(1);
                    }
                    None => *next = None,
                }
            },
        }
    }
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
            trails: patterns
                .iter()
                .map(|part| Trail {
                    part,
                    nodes: Vec::new(),
                    relationships: Vec::new(),
                    split: 0,
                })
                .collect(),
            fixed: Fixed::default(),
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
        #[cfg(test)]
        tests::evaluated();
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
            .map(|(key, value)| Ok((self.property_key(&key)?, value)))
            .collect()
    }

    fn with_expected<'p, R>(
        &self,
        expected: &'p Expr,
        row: &Row,
        fixed: Option<&Fixed<'p>>,
        test: impl FnOnce(&ExpectedProperties) -> Result<R, Error>,
    ) -> Result<R, Error> {
        let Some(fixed) = fixed.filter(|_| row_free(expected)) else {
            return test(&self.expected_properties(expected, row)?);
        };
        let held = fixed
            .0
            .borrow()
            .iter()
            .position(|(held, _)| std::ptr::eq(*held, expected));
        let index = match held {
            Some(index) => index,
            None => {
                let properties = self.expected_properties(expected, row)?;
                let mut maps = fixed.0.borrow_mut();
                maps.push((expected, properties));
                maps.len() - 1
            }
        };
        test(&fixed.0.borrow()[index].1)
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

    fn node_matches<'p>(
        &self,
        id: NodeId,
        pattern: &'p NodePattern,
        known: Known,
        row: &Row,
        fixed: Option<&Fixed<'p>>,
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
        self.with_expected(expected, row, fixed, |expected| {
            holds(expected, known.served, |key| {
                Ok(self.node_property(id, Some(key))?.flatten())
            })
        })
    }

    fn relationship_matches<'p>(
        &self,
        id: RelationshipId,
        pattern: &'p RelationshipPattern,
        row: &Row,
        fixed: &Fixed<'p>,
    ) -> Result<bool, Error> {
        let Some(expected) = &pattern.properties else {
            return Ok(true);
        };
        self.with_expected(expected, row, Some(fixed), |expected| {
            let held = self.tx.with_relationship(id, |bytes| {
                holds(expected, None, |key| {
                    Ok(RelationshipRecord::property(bytes, key)?)
                })
            })?;
            Ok(held == Some(true))
        })
    }

    fn start_candidates<'p>(
        &self,
        part: &'p PatternPart,
        row: &Row,
    ) -> Result<(&'p PatternPart, Candidates, Known), Error> {
        if let Some(ids) = self.morsel_of(part) {
            let known = Known {
                labels: label_scan_decides(&part.start),
                served: None,
            };
            return Ok((part, Candidates::Listed(Vec::from(ids).into_iter()), known));
        }
        let (part, sought) = self.oriented(part, row)?;
        let pattern = &part.start;
        if pattern.bound {
            let nodes = match row.get(pattern.slot) {
                Some(Datum::Node(id)) => vec![*id],
                _ => Vec::new(),
            };
            return Ok((part, Candidates::Listed(nodes.into_iter()), Known::NOTHING));
        }
        let labels_known = label_scan_decides(pattern);
        if let Some(sought) = sought {
            let known = Known {
                labels: labels_known && sought.labelled,
                served: sought.served,
            };
            return Ok((part, Candidates::Listed(sought.nodes.into_iter()), known));
        }
        let known = Known {
            labels: labels_known,
            served: None,
        };
        let candidates = match self.scanned_label(pattern)? {
            Some(label) => Candidates::Scanned {
                label,
                group: Vec::new().into_iter(),
                next: Some(0),
            },
            None => Candidates::Listed(Vec::new().into_iter()),
        };
        Ok((part, candidates, known))
    }

    /// The order to walk `part` in from `row`, and the start nodes a lookup found for it. The
    /// part begins at the leftmost of its starts that has the fewest start nodes: as many as
    /// a lookup finds, or else as many as the start's label holds. A later start's lookup
    /// stops reading index entries once it reaches the fewest found so far, and none is tried
    /// once a start has at most one node.
    pub(crate) fn oriented<'p>(
        &self,
        part: &'p PatternPart,
        row: &Row,
    ) -> Result<(&'p PatternPart, Option<Sought>), Error> {
        let sought = self.seek_start(part, row)?;
        if part.others.is_empty() {
            return Ok((part, sought));
        }
        let mut fewest = match &sought {
            Some(sought) => sought.nodes.len(),
            None => usize::try_from(self.scanned_count(&part.start)?).unwrap_or(usize::MAX),
        };
        let mut chosen = (part, sought);
        for other in &part.others {
            if fewest <= 1 {
                break;
            }
            if let Some(found) = self.seek_within(other, row, fewest - 1)? {
                fewest = found.nodes.len();
                chosen = (other, Some(found));
            }
        }
        Ok(chosen)
    }

    /// Whether [`Env::oriented`] may begin `part`, whose leftmost start no lookup serves, at
    /// another of its starts.
    pub(crate) fn may_move(&self, part: &PatternPart) -> Result<bool, Error> {
        Ok(self.others_served(part)? && self.scanned_count(&part.start)? > 1)
    }

    fn others_served(&self, part: &PatternPart) -> Result<bool, Error> {
        for other in &part.others {
            if self.seek_served(other)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// How many nodes [`Env::scanned_nodes`] gives, read from the node counts once per
    /// statement.
    pub(crate) fn scanned_count(&self, pattern: &NodePattern) -> Result<u64, Error> {
        let label = match pattern.labels.as_ref().and_then(LabelExpr::required) {
            Some(label) => match self.label_token(&label.0)? {
                Some(token) => Some(token),
                None => return Ok(0),
            },
            None => None,
        };
        if let Some((_, count)) = self
            .node_counts
            .borrow()
            .iter()
            .find(|(held, _)| *held == label)
        {
            return Ok(*count);
        }
        let count = match label {
            Some(token) => self.tx.count_nodes_with_label(token)?,
            None => self.tx.count_nodes()?,
        };
        self.node_counts.borrow_mut().push((label, count));
        Ok(count)
    }

    /// The nodes a pattern starts from when no seek serves it: those of its required label, or
    /// every node, in ascending ID order.
    pub(crate) fn scanned_nodes(&self, pattern: &NodePattern) -> Result<Vec<NodeId>, Error> {
        Ok(match self.scanned_label(pattern)? {
            Some(Some(label)) => self.tx.node_ids_with_label(label)?,
            Some(None) => self.tx.node_ids()?,
            None => Vec::new(),
        })
    }

    /// The label whose nodes [`Env::scanned_nodes`] gives, or none for every node, or `None`
    /// when no node carries the required label.
    pub(crate) fn scanned_label(
        &self,
        pattern: &NodePattern,
    ) -> Result<Option<Option<Token<Label>>>, Error> {
        Ok(
            match pattern.labels.as_ref().and_then(LabelExpr::required) {
                Some(label) => self.label_token(&label.0)?.map(Some),
                None => Some(None),
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
        self.node_matches(id, pattern, Known::NOTHING, row, None)
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
                    &mut self.caches.adjacency.borrow_mut(),
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
        let adjacent = self.tx.adjacent_with(
            &mut self.caches.adjacency.borrow_mut(),
            node,
            direction,
            None,
        )?;
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

fn holds<E>(
    expected: &ExpectedProperties,
    skipped: Option<Token<PropertyKey>>,
    stored: impl Fn(Token<PropertyKey>) -> Result<Option<PropertyValue>, E>,
) -> Result<bool, E> {
    for (key, value) in expected {
        if key.is_some() && *key == skipped {
            continue;
        }
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
                    match candidates.next(self.env)? {
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
                                    &self.trails[index].part.steps[step_index].relationship;
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
            #[cfg(test)]
            super::stream::produced();
            return emit(row);
        };
        let (walked, candidates, known) = self.env.start_candidates(part, row)?;
        self.trails[index].part = walked;
        frames.push(Frame::Part {
            index,
            candidates,
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
        let start = &self.trails[index].part.start;
        if !self
            .env
            .node_matches(id, start, known, row, Some(&self.fixed))?
        {
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
        let part = self.trails[index].part;
        let current = if step_index == part.left && step_index > 0 {
            self.trails[index].split = self.trails[index].relationships.len();
            self.trails[index].nodes[0]
        } else {
            current
        };
        let Some(step) = part.steps.get(step_index) else {
            if let Some(path) = part.path {
                set(row, path, Datum::Path(self.path(index)));
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
        let part = self.trails[index].part;
        let relationship = &part.steps[step_index].relationship;
        for adjacent in adjacents {
            if self.used.contains(&adjacent.relationship) {
                continue;
            }
            if relationship.bound
                && row.get(relationship.slot) != Some(&Datum::Relationship(adjacent.relationship))
            {
                continue;
            }
            if !self.env.relationship_matches(
                adjacent.relationship,
                relationship,
                row,
                &self.fixed,
            )? {
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
        let part = self.trails[index].part;
        let relationship = &part.steps[step_index].relationship;
        for adjacent in adjacents {
            if !self.used.contains(&adjacent.relationship)
                && self.env.relationship_matches(
                    adjacent.relationship,
                    relationship,
                    row,
                    &self.fixed,
                )?
            {
                return Ok(Some(adjacent));
            }
        }
        Ok(None)
    }

    fn path(&self, index: usize) -> PathIds {
        let trail = &self.trails[index];
        if trail.part.left == 0 {
            return PathIds {
                nodes: trail.nodes.clone(),
                relationships: trail.relationships.clone(),
            };
        }
        let split = trail.split;
        let (left, right) = trail.nodes.split_at(split + 1);
        let (left_relationships, right_relationships) = trail.relationships.split_at(split);
        PathIds {
            nodes: left[1..]
                .iter()
                .rev()
                .chain(&left[..1])
                .chain(right)
                .copied()
                .collect(),
            relationships: left_relationships
                .iter()
                .rev()
                .chain(right_relationships)
                .copied()
                .collect(),
        }
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
        let part = self.trails[index].part;
        let relationship = &part.steps[step_index].relationship;
        if !relationship.anonymous {
            let walked = self.trails[index].relationships[base..]
                .iter()
                .copied()
                .map(Datum::Relationship);
            let value = Datum::list(if step_index < part.left {
                walked.rev().collect()
            } else {
                walked.collect()
            });
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
        let part = self.trails[index].part;
        let pattern = &part.steps[step_index].node;
        if pattern.bound {
            if row.get(pattern.slot) != Some(&Datum::Node(node)) {
                return Ok(true);
            }
        } else {
            set(row, pattern.slot, Datum::Node(node));
        }
        if !self
            .env
            .node_matches(node, pattern, Known::NOTHING, row, Some(&self.fixed))?
        {
            return Ok(true);
        }
        self.begin_step(index, step_index + 1, node, row, frames, emit)
    }
}

#[cfg(test)]
#[path = "matcher.test.rs"]
mod tests;
