use std::collections::HashMap;
use std::collections::HashSet;
use std::hash::BuildHasherDefault;
use std::hash::Hasher;
use std::sync::Arc;

use crate::error::Error;
use crate::graph::Adjacent;
use crate::graph::NodeId;
use crate::graph::RelationshipId;
use crate::storage::ReadTransaction;

use super::datum::Datum;
use super::datum::PathIds;
use super::eval::Env;
use super::eval::Row;
use super::eval::set;
use super::plan::Group;
use super::plan::Member;
use super::plan::NodePattern;
use super::plan::PathMode;
use super::plan::PathSearch;
use super::plan::PatternPart;
use super::plan::Segment;
use super::plan::Selector;
use super::plan::Step;

/// One path a search found, and how many times it repeats each segment that is a group.
/// The counts are empty when the search is one group of one relationship, which repeats
/// once per relationship of the path.
#[derive(Clone, Debug)]
pub(crate) struct Found {
    pub(crate) trail: PathIds,
    pub(crate) counts: Vec<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Point {
    /// Before segment `index`, at a node that the pattern before it matched.
    Before(usize),
    /// In group `index` after `count` iterations, where an unbounded group counts only up to
    /// its minimum.
    Boundary(usize, u64),
    /// In group `index` after `count` iterations and `hop` hops of the next.
    Inside(usize, u64, usize),
}

/// A hasher for keys that are already well spread, such as element IDs.
#[derive(Default)]
pub(crate) struct IdHasher(u64);

impl Hasher for IdHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 = (self.0.rotate_left(8) ^ u64::from(*byte)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        }
    }

    fn write_u64(&mut self, value: u64) {
        self.0 = (self.0 ^ value).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }

    fn write_usize(&mut self, value: usize) {
        self.write_u64(value as u64);
    }
}

pub(crate) type IdMap<K, V> = HashMap<K, V, BuildHasherDefault<IdHasher>>;

type IdSet = HashSet<NodeId, BuildHasherDefault<IdHasher>>;

struct Space<'s, 'e, T: ReadTransaction> {
    env: &'s Env<'e, T>,
    search: &'s PathSearch,
    row: Row,
    members: Vec<(usize, IdSet)>,
}

impl<T: ReadTransaction> Env<'_, T> {
    /// The paths of `part` from `start` that its search keeps, ending at one of `targets` when
    /// given, and using none of `excluded`.
    pub(crate) fn search_paths(
        &self,
        part: &PatternPart,
        search: &PathSearch,
        start: NodeId,
        targets: Option<&[NodeId]>,
        excluded: &[crate::graph::RelationshipId],
        row: &Row,
    ) -> Result<Vec<Found>, Error> {
        let mut row = row.clone();
        set(&mut row, part.start.slot, Datum::Node(start));
        let mut space = Space {
            env: self,
            search,
            row,
            members: Vec::new(),
        };
        let found = match search.selector {
            Selector::Paths(0) | Selector::Groups(0) => Vec::new(),
            Selector::All => space.every(part, start, targets, excluded)?,
            selector => {
                if let Some(found) = space.single_hop(start, targets, selector)? {
                    found
                } else if search.mode == PathMode::Walk {
                    space.breadth_first(start, targets, selector)?
                } else {
                    space.deepening(part, start, targets, selector)?
                }
            }
        };
        Ok(found)
    }

    /// The nodes the paths of `search` may end at, when the end node is bound or a seek finds
    /// them.
    pub(crate) fn path_targets(
        &self,
        part: &PatternPart,
        search: &PathSearch,
        row: &Row,
    ) -> Result<Option<Vec<NodeId>>, Error> {
        let Some(end) = search.end() else {
            return Ok(None);
        };
        if end.bound {
            if end.slot == part.start.slot {
                return Ok(None);
            }
            return Ok(Some(match row.get(end.slot) {
                Some(Datum::Node(id)) => vec![*id],
                _ => Vec::new(),
            }));
        }
        let Some(target) = &search.target else {
            return Ok(None);
        };
        Ok(self.seek_start(target, row)?.map(|sought| sought.nodes))
    }

    /// Sets the slots that `found` binds as a match of `part`.
    pub(crate) fn bind_found(
        &self,
        part: &PatternPart,
        search: &PathSearch,
        found: Found,
        row: &mut Row,
    ) {
        let nodes = &found.trail.nodes;
        let relationships = &found.trail.relationships;
        let node = |at: usize| nodes.get(at).map_or(Datum::Null, |id| Datum::Node(*id));
        let relationship = |at: usize| {
            relationships
                .get(at)
                .map_or(Datum::Null, |id| Datum::Relationship(*id))
        };
        if !part.start.bound {
            set(row, part.start.slot, node(0));
        }
        let mut offset = 0;
        for (index, segment) in search.segments.iter().enumerate() {
            match segment {
                Segment::Hop(step) => {
                    if !step.relationship.bound {
                        set(row, step.relationship.slot, relationship(offset));
                    }
                    offset += 1;
                    if !step.node.bound {
                        set(row, step.node.slot, node(offset));
                    }
                }
                Segment::Group(group) => {
                    let width = group.hops.len().max(1);
                    let count = match found.counts.get(index) {
                        Some(count) => usize::try_from(*count).unwrap_or(usize::MAX),
                        None => relationships.len() / width,
                    };
                    for (member, slot) in &group.lists {
                        let items = (0..count)
                            .map(|iteration| {
                                let base = offset + iteration * width;
                                match member {
                                    Member::Entry => node(base),
                                    Member::Relationship(hop) => relationship(base + hop),
                                    Member::Node(hop) => node(base + hop + 1),
                                }
                            })
                            .collect();
                        set(row, *slot, Datum::List(Arc::new(items)));
                    }
                    offset += count * width;
                    if !group.exit.bound {
                        set(row, group.exit.slot, node(offset));
                    }
                }
            }
        }
        if let Some(path) = part.path {
            set(row, path, Datum::Path(found.trail));
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Pick {
    paths: u64,
    lengths: u64,
    last: Option<usize>,
}

impl Pick {
    fn admit(&mut self, selector: Selector, length: usize) -> Option<Option<u64>> {
        match selector {
            Selector::All => Some(None),
            Selector::Paths(count) => (self.paths < count).then(|| Some(count - self.paths)),
            Selector::Groups(count) => {
                if self.last == Some(length) {
                    return Some(None);
                }
                if self.lengths >= count {
                    return None;
                }
                self.lengths += 1;
                self.last = Some(length);
                Some(None)
            }
        }
    }

    fn done(&self, selector: Selector) -> bool {
        match selector {
            Selector::All => false,
            Selector::Paths(count) => self.paths >= count,
            Selector::Groups(count) => self.lengths >= count,
        }
    }
}

fn trivial(pattern: &NodePattern) -> bool {
    !pattern.bound && pattern.labels.is_none() && pattern.properties.is_none()
}

fn wanted(targets: Option<&[NodeId]>, node: NodeId) -> bool {
    targets.is_none_or(|targets| targets.contains(&node))
}

fn all_done(targets: Option<&[NodeId]>, picks: &IdMap<NodeId, Pick>, selector: Selector) -> bool {
    targets.is_some_and(|targets| {
        targets
            .iter()
            .all(|target| picks.get(target).is_some_and(|pick| pick.done(selector)))
    })
}

struct Frame {
    moves: Vec<(Point, Adjacent)>,
    next: usize,
    extended: bool,
}

struct Walk {
    nodes: Vec<NodeId>,
    relationships: Vec<RelationshipId>,
    segments: Vec<usize>,
}

impl Walk {
    fn found(&self, search: &PathSearch) -> Found {
        Found {
            trail: PathIds {
                nodes: self.nodes.clone(),
                relationships: self.relationships.clone(),
            },
            counts: counts(search, self.segments.iter().copied()),
        }
    }
}

fn counts(search: &PathSearch, segments: impl Iterator<Item = usize>) -> Vec<u64> {
    let mut counts = vec![0u64; search.segments.len()];
    for segment in segments {
        if let Some(count) = counts.get_mut(segment) {
            *count += 1;
        }
    }
    for (count, segment) in counts.iter_mut().zip(&search.segments) {
        if let Segment::Group(group) = segment {
            *count /= group.hops.len().max(1) as u64;
        }
    }
    counts
}

impl<T: ReadTransaction> Space<'_, '_, T> {
    fn node_ok(&self, node: NodeId, pattern: &NodePattern) -> Result<bool, Error> {
        if pattern.bound && self.row.get(pattern.slot) != Some(&Datum::Node(node)) {
            return Ok(false);
        }
        if trivial(pattern) {
            return Ok(true);
        }
        if let Some((_, members)) = self.members.iter().find(|(slot, _)| *slot == pattern.slot) {
            return Ok(members.contains(&node));
        }
        self.env.node_fits(node, pattern, &self.row)
    }

    fn learn_members(&mut self, pattern: &NodePattern) -> Result<(), Error> {
        let decided = !pattern.bound
            && pattern.properties.is_none()
            && matches!(pattern.labels, Some(crate::gql::ast::LabelExpr::Name(_)));
        if decided && !self.members.iter().any(|(slot, _)| *slot == pattern.slot) {
            let members = self.env.scanned_nodes(pattern)?.into_iter().collect();
            self.members.push((pattern.slot, members));
        }
        Ok(())
    }

    fn group(&self, index: usize) -> Option<&Group> {
        match self.search.segments.get(index) {
            Some(Segment::Group(group)) => Some(group),
            _ => None,
        }
    }

    fn settle(&self, point: Point, node: NodeId) -> Result<(bool, Vec<Point>), Error> {
        let mut pending = vec![point];
        let mut accepting = false;
        let mut expandable = Vec::new();
        while let Some(point) = pending.pop() {
            match point {
                Point::Before(index) => match self.search.segments.get(index) {
                    None => accepting = true,
                    Some(Segment::Hop(_)) => expandable.push(point),
                    Some(Segment::Group(_)) => pending.push(Point::Boundary(index, 0)),
                },
                Point::Boundary(index, count) => {
                    let Some(group) = self.group(index) else {
                        continue;
                    };
                    if group.max.is_none_or(|max| count < max)
                        && self.node_ok(node, &group.entry)?
                    {
                        expandable.push(point);
                    }
                    if count >= group.min && self.node_ok(node, &group.exit)? {
                        pending.push(Point::Before(index + 1));
                    }
                }
                Point::Inside(..) => expandable.push(point),
            }
        }
        Ok((accepting, expandable))
    }

    fn step(&self, point: Point) -> Option<&Step> {
        match point {
            Point::Before(index) => match self.search.segments.get(index) {
                Some(Segment::Hop(step)) => Some(step),
                _ => None,
            },
            Point::Boundary(index, _) => self.group(index)?.hops.first(),
            Point::Inside(index, _, hop) => self.group(index)?.hops.get(hop),
        }
    }

    fn moves(&self, node: NodeId, points: &[Point]) -> Result<Vec<(Point, Adjacent)>, Error> {
        let mut moves = Vec::new();
        let mut found = Vec::new();
        for point in points {
            let Some(step) = self.step(*point) else {
                continue;
            };
            found.clear();
            let read = self.env.hop_read(&step.relationship)?;
            self.env
                .hop_into(node, &step.relationship, read, &mut found)?;
            moves.extend(found.iter().map(|adjacent| (*point, *adjacent)));
        }
        Ok(moves)
    }

    fn hop_ok(&self, point: Point, adjacent: &Adjacent) -> Result<bool, Error> {
        let Some(step) = self.step(point) else {
            return Ok(false);
        };
        let relationship = &step.relationship;
        if relationship.bound
            && self.row.get(relationship.slot) != Some(&Datum::Relationship(adjacent.relationship))
        {
            return Ok(false);
        }
        Ok(self
            .env
            .relationship_matches(adjacent.relationship, relationship, &self.row)?
            && self.node_ok(adjacent.other, &step.node)?)
    }

    fn segment_of(point: Point) -> usize {
        match point {
            Point::Before(index) | Point::Boundary(index, _) | Point::Inside(index, _, _) => index,
        }
    }

    fn after(
        &mut self,
        point: Point,
        iteration: (&[NodeId], &[RelationshipId]),
    ) -> Result<Option<Point>, Error> {
        let (index, count, hop) = match point {
            Point::Before(index) => return Ok(Some(Point::Before(index + 1))),
            Point::Boundary(index, count) => (index, count, 0),
            Point::Inside(index, count, hop) => (index, count, hop),
        };
        let search = self.search;
        let Some(Segment::Group(group)) = search.segments.get(index) else {
            return Ok(None);
        };
        if hop + 1 < group.hops.len() {
            return Ok(Some(Point::Inside(index, count, hop + 1)));
        }
        let next = if group.max.is_none() {
            (count + 1).min(group.min)
        } else {
            count + 1
        };
        if let Some(filter) = &group.filter {
            let (nodes, relationships) = iteration;
            if let Some(entry) = nodes.first() {
                set(&mut self.row, group.entry.slot, Datum::Node(*entry));
            }
            for (at, step) in group.hops.iter().enumerate() {
                if let Some(id) = relationships.get(at) {
                    set(
                        &mut self.row,
                        step.relationship.slot,
                        Datum::Relationship(*id),
                    );
                }
                if let Some(id) = nodes.get(at + 1) {
                    set(&mut self.row, step.node.slot, Datum::Node(*id));
                }
            }
            if !self.env.predicate(filter, &self.row)? {
                return Ok(None);
            }
        }
        Ok(Some(Point::Boundary(index, next)))
    }

    fn into_iteration(point: Point) -> usize {
        match point {
            Point::Inside(_, _, hop) => hop,
            Point::Before(_) | Point::Boundary(..) => 0,
        }
    }

    fn mode_allows(&self, walk: &Walk, adjacent: &Adjacent, excluded: &[RelationshipId]) -> bool {
        if excluded.contains(&adjacent.relationship) {
            return false;
        }
        match self.search.mode {
            PathMode::Walk => true,
            PathMode::Trail => !walk.relationships.contains(&adjacent.relationship),
            PathMode::Acyclic => !walk.nodes.contains(&adjacent.other),
            PathMode::Simple => !walk
                .nodes
                .get(1..)
                .is_some_and(|rest| rest.contains(&adjacent.other)),
        }
    }

    fn depth_first(
        &mut self,
        start: NodeId,
        budget: Option<usize>,
        excluded: &[RelationshipId],
        emit: &mut dyn FnMut(&mut Self, &Walk) -> Result<(), Error>,
    ) -> Result<bool, Error> {
        let mut walk = Walk {
            nodes: vec![start],
            relationships: Vec::new(),
            segments: Vec::new(),
        };
        let mut reached = false;
        let (accepting, expandable) = self.settle(Point::Before(0), start)?;
        if accepting && budget.is_none_or(|budget| budget == 0) {
            emit(self, &walk)?;
        }
        if budget == Some(0) {
            return Ok(!expandable.is_empty());
        }
        let mut frames = vec![Frame {
            moves: self.moves(start, &expandable)?,
            next: 0,
            extended: false,
        }];
        while let Some(frame) = frames.last_mut() {
            if std::mem::take(&mut frame.extended) {
                walk.nodes.pop();
                walk.relationships.pop();
                walk.segments.pop();
            }
            let Some((point, adjacent)) = frame.moves.get(frame.next).copied() else {
                frames.pop();
                continue;
            };
            frame.next += 1;
            if !self.mode_allows(&walk, &adjacent, excluded) || !self.hop_ok(point, &adjacent)? {
                continue;
            }
            let depth = walk.relationships.len();
            let begun = depth - Self::into_iteration(point).min(depth);
            let mut nodes = walk.nodes[begun..].to_vec();
            nodes.push(adjacent.other);
            let mut relationships = walk.relationships[begun..].to_vec();
            relationships.push(adjacent.relationship);
            let Some(next) = self.after(point, (&nodes, &relationships))? else {
                continue;
            };
            frame.extended = true;
            walk.nodes.push(adjacent.other);
            walk.relationships.push(adjacent.relationship);
            walk.segments.push(Self::segment_of(point));
            let closed =
                self.search.mode == PathMode::Simple && walk.nodes.first() == Some(&adjacent.other);
            let (accepting, mut expandable) = self.settle(next, adjacent.other)?;
            if closed {
                expandable.clear();
            }
            if accepting && budget.is_none_or(|budget| budget == depth + 1) {
                emit(self, &walk)?;
            }
            if budget == Some(depth + 1) {
                reached |= !expandable.is_empty();
                continue;
            }
            let moves = self.moves(adjacent.other, &expandable)?;
            frames.push(Frame {
                moves,
                next: 0,
                extended: false,
            });
        }
        Ok(reached)
    }

    fn filter_ok(&mut self, part: &PatternPart, found: &Found) -> Result<bool, Error> {
        let Some(filter) = &self.search.filter else {
            return Ok(true);
        };
        let mut row = self.row.clone();
        self.env
            .bind_found(part, self.search, found.clone(), &mut row);
        self.env.predicate(filter, &row)
    }

    fn every(
        &mut self,
        part: &PatternPart,
        start: NodeId,
        targets: Option<&[NodeId]>,
        excluded: &[RelationshipId],
    ) -> Result<Vec<Found>, Error> {
        let mut kept = Vec::new();
        self.depth_first(start, None, excluded, &mut |space, walk| {
            let end = walk.nodes.last().copied().unwrap_or(start);
            if wanted(targets, end) {
                let found = walk.found(space.search);
                if space.filter_ok(part, &found)? {
                    kept.push(found);
                }
            }
            Ok(())
        })?;
        Ok(kept)
    }

    fn deepening(
        &mut self,
        part: &PatternPart,
        start: NodeId,
        targets: Option<&[NodeId]>,
        selector: Selector,
    ) -> Result<Vec<Found>, Error> {
        let mut kept = Vec::new();
        let mut picks: IdMap<NodeId, Pick> = IdMap::default();
        let mut length = 0;
        loop {
            let reached = self.depth_first(start, Some(length), &[], &mut |space, walk| {
                let end = walk.nodes.last().copied().unwrap_or(start);
                if !wanted(targets, end) {
                    return Ok(());
                }
                let found = walk.found(space.search);
                if !space.filter_ok(part, &found)? {
                    return Ok(());
                }
                let pick = picks.entry(end).or_default();
                if pick.admit(selector, length).is_some() {
                    if let Selector::Paths(_) = selector {
                        pick.paths += 1;
                    }
                    kept.push(found);
                }
                Ok(())
            })?;
            if !reached || all_done(targets, &picks, selector) {
                return Ok(kept);
            }
            length += 1;
        }
    }
}

struct Entry {
    node: NodeId,
    point: Point,
    key: usize,
    parents: Vec<(usize, RelationshipId)>,
    paths: u64,
    iteration: Arc<(Vec<NodeId>, Vec<RelationshipId>)>,
}

#[derive(Default)]
struct KeyState {
    lengths: Vec<(usize, usize)>,
    paths: u64,
}

type Key = (NodeId, Point, Arc<(Vec<NodeId>, Vec<RelationshipId>)>);

impl<T: ReadTransaction> Space<'_, '_, T> {
    fn keeps_iteration(&self, point: Point) -> bool {
        match point {
            Point::Inside(index, ..) => self
                .group(index)
                .is_some_and(|group| group.filter.is_some() && group.hops.len() > 1),
            Point::Before(_) | Point::Boundary(..) => false,
        }
    }

    fn breadth_first(
        &mut self,
        start: NodeId,
        targets: Option<&[NodeId]>,
        selector: Selector,
    ) -> Result<Vec<Found>, Error> {
        let cap = match selector {
            Selector::Paths(count) => count,
            Selector::Groups(_) | Selector::All => u64::MAX,
        };
        let empty = Arc::new((Vec::new(), Vec::new()));
        let mut keys: IdMap<Key, usize> = IdMap::default();
        let mut states: Vec<KeyState> = vec![KeyState {
            lengths: vec![(0, 0)],
            paths: 1,
        }];
        keys.insert((start, Point::Before(0), empty.clone()), 0);
        let mut entries = vec![Entry {
            node: start,
            point: Point::Before(0),
            key: 0,
            parents: Vec::new(),
            paths: 1,
            iteration: empty.clone(),
        }];
        let mut layer = vec![0usize];
        let mut picks: IdMap<NodeId, Pick> = IdMap::default();
        let mut finals: Vec<(usize, Option<u64>)> = Vec::new();
        let mut length = 0;
        while !layer.is_empty() {
            let mut next: Vec<usize> = Vec::new();
            for &at in &layer {
                let (node, point) = (entries[at].node, entries[at].point);
                let (accepting, expandable) = self.settle(point, node)?;
                if accepting && wanted(targets, node) {
                    let pick = picks.entry(node).or_default();
                    if let Some(take) = pick.admit(selector, length) {
                        if let Selector::Paths(_) = selector {
                            pick.paths = pick.paths.saturating_add(entries[at].paths);
                        }
                        finals.push((at, take));
                    }
                }
                for (from, adjacent) in self.moves(node, &expandable)? {
                    if !self.hop_ok(from, &adjacent)? {
                        continue;
                    }
                    let (mut nodes, mut relationships) = match from {
                        Point::Inside(..) => (
                            entries[at].iteration.0.clone(),
                            entries[at].iteration.1.clone(),
                        ),
                        Point::Before(_) | Point::Boundary(..) => (Vec::new(), Vec::new()),
                    };
                    nodes.push(node);
                    relationships.push(adjacent.relationship);
                    let mut walked = nodes.clone();
                    walked.push(adjacent.other);
                    let Some(arrived) = self.after(from, (&walked, &relationships))? else {
                        continue;
                    };
                    let iteration = if self.keeps_iteration(arrived) {
                        Arc::new((nodes, relationships))
                    } else {
                        empty.clone()
                    };
                    let key = (adjacent.other, arrived, iteration.clone());
                    let state = match keys.get(&key) {
                        Some(state) => *state,
                        None => {
                            states.push(KeyState::default());
                            keys.insert(key, states.len() - 1);
                            states.len() - 1
                        }
                    };
                    let current = states[state].lengths.last().copied();
                    if let Some((at_length, entry)) = current
                        && at_length == length + 1
                    {
                        entries[entry].parents.push((at, adjacent.relationship));
                        continue;
                    }
                    let open = match selector {
                        Selector::Groups(count) => (states[state].lengths.len() as u64) < count,
                        Selector::Paths(count) => states[state].paths < count,
                        Selector::All => true,
                    };
                    if !open {
                        continue;
                    }
                    entries.push(Entry {
                        node: adjacent.other,
                        point: arrived,
                        key: state,
                        parents: vec![(at, adjacent.relationship)],
                        paths: 0,
                        iteration,
                    });
                    let entry = entries.len() - 1;
                    states[state].lengths.push((length + 1, entry));
                    next.push(entry);
                }
            }
            for &entry in &next {
                let paths = entries[entry]
                    .parents
                    .iter()
                    .fold(0u64, |sum, (parent, _)| {
                        sum.saturating_add(entries[*parent].paths)
                    })
                    .min(cap);
                entries[entry].paths = paths;
                let key = entries[entry].key;
                states[key].paths = states[key].paths.saturating_add(paths);
            }
            if all_done(targets, &picks, selector) {
                break;
            }
            layer = next;
            length += 1;
        }
        let mut kept = Vec::new();
        for (entry, take) in finals {
            unfold(&entries, entry, take, self.search, &mut kept);
        }
        Ok(kept)
    }
}

fn unfold(
    entries: &[Entry],
    entry: usize,
    take: Option<u64>,
    search: &PathSearch,
    kept: &mut Vec<Found>,
) {
    let mut stack: Vec<(usize, usize)> = vec![(entry, 0)];
    let mut made = 0u64;
    while let Some(&(top, next)) = stack.last() {
        let parents = entries.get(top).map_or(&[][..], |entry| &entry.parents[..]);
        if parents.is_empty() {
            let nodes = stack
                .iter()
                .rev()
                .filter_map(|(at, _)| entries.get(*at).map(|entry| entry.node))
                .collect();
            let mut relationships = Vec::with_capacity(stack.len());
            let mut segments = Vec::with_capacity(stack.len());
            for (child, chosen) in stack.iter().rev().skip(1) {
                let Some(child) = entries.get(*child) else {
                    continue;
                };
                if let Some((_, relationship)) =
                    chosen.checked_sub(1).and_then(|at| child.parents.get(at))
                {
                    relationships.push(*relationship);
                    segments.push(match child.point {
                        Point::Before(index) => index.saturating_sub(1),
                        Point::Boundary(index, _) | Point::Inside(index, _, _) => index,
                    });
                }
            }
            kept.push(Found {
                trail: PathIds {
                    nodes,
                    relationships,
                },
                counts: counts(search, segments.into_iter()),
            });
            made += 1;
            if take.is_some_and(|take| made >= take) {
                return;
            }
            stack.pop();
            continue;
        }
        match parents.get(next) {
            Some((parent, _)) => {
                let parent = *parent;
                if let Some(top) = stack.last_mut() {
                    top.1 += 1;
                }
                stack.push((parent, 0));
            }
            None => {
                stack.pop();
            }
        }
    }
}

struct Tree {
    at: IdMap<NodeId, usize>,
    reached: Vec<Reached>,
}

impl Tree {
    fn first_path(&self, index: usize) -> Option<(Vec<NodeId>, Vec<RelationshipId>)> {
        let layer = self.reached.get(index)?.layer;
        let mut nodes = Vec::with_capacity(layer + 1);
        let mut relationships = Vec::with_capacity(layer);
        let mut at = index;
        loop {
            let reached = self.reached.get(at)?;
            nodes.push(reached.node);
            match reached.parents.first() {
                Some((parent, relationship)) => {
                    relationships.push(*relationship);
                    at = *parent;
                }
                None => break,
            }
        }
        nodes.reverse();
        relationships.reverse();
        Some((nodes, relationships))
    }
}

struct Reached {
    node: NodeId,
    layer: usize,
    parents: Vec<(usize, RelationshipId)>,
}

impl Tree {
    fn new(root: NodeId) -> Self {
        let mut at = IdMap::default();
        at.insert(root, 0);
        Self {
            at,
            reached: vec![Reached {
                node: root,
                layer: 0,
                parents: Vec::new(),
            }],
        }
    }

    fn layer_of(&self, node: NodeId) -> Option<usize> {
        let at = *self.at.get(&node)?;
        self.reached.get(at).map(|reached| reached.layer)
    }

    fn reach(
        &mut self,
        node: NodeId,
        layer: usize,
        parent: usize,
        relationship: RelationshipId,
        every: bool,
    ) -> bool {
        match self.at.get(&node) {
            Some(&at) => {
                if let Some(reached) = self.reached.get_mut(at)
                    && every
                    && reached.layer == layer
                {
                    reached.parents.push((parent, relationship));
                }
                false
            }
            None => {
                self.at.insert(node, self.reached.len());
                self.reached.push(Reached {
                    node,
                    layer,
                    parents: vec![(parent, relationship)],
                });
                true
            }
        }
    }

    fn sort_layer(&self, layer: &mut [usize]) {
        let keyed: Vec<(u64, usize)> = layer
            .iter()
            .filter_map(|at| self.reached.get(*at).map(|reached| (reached.node.0, *at)))
            .collect();
        for (slot, (_, at)) in layer.iter_mut().zip(radix_sorted(keyed)) {
            *slot = at;
        }
    }

    fn paths(&self, index: usize, every: bool) -> Vec<(Vec<NodeId>, Vec<RelationshipId>)> {
        if !every {
            return self.first_path(index).into_iter().collect();
        }
        let mut paths = Vec::new();
        let mut stack: Vec<(usize, usize)> = vec![(index, 0)];
        while let Some(&(top, next)) = stack.last() {
            let parents = self
                .reached
                .get(top)
                .map_or(&[][..], |reached| &reached.parents[..]);
            if parents.is_empty() {
                let nodes = stack
                    .iter()
                    .rev()
                    .filter_map(|(at, _)| self.reached.get(*at).map(|reached| reached.node))
                    .collect();
                let relationships = stack
                    .iter()
                    .rev()
                    .skip(1)
                    .filter_map(|(child, chosen)| {
                        let child = self.reached.get(*child)?;
                        chosen
                            .checked_sub(1)
                            .and_then(|at| child.parents.get(at))
                            .map(|(_, relationship)| *relationship)
                    })
                    .collect();
                paths.push((nodes, relationships));
                if !every {
                    return paths;
                }
                stack.pop();
                continue;
            }
            match parents.get(next) {
                Some((parent, _)) => {
                    let parent = *parent;
                    if let Some(top) = stack.last_mut() {
                        top.1 += 1;
                    }
                    stack.push((parent, 0));
                }
                None => {
                    stack.pop();
                }
            }
        }
        paths
    }
}

fn radix_sorted(mut keyed: Vec<(u64, usize)>) -> Vec<(u64, usize)> {
    if keyed.len() < 64 {
        keyed.sort_unstable();
        return keyed;
    }
    let highest = keyed.iter().fold(0u64, |bits, (key, _)| bits | key);
    let bytes = (u64::BITS - highest.leading_zeros()).div_ceil(8);
    let mut spare = keyed.clone();
    for byte in 0..bytes {
        let shift = byte * 8;
        let mut starts = [0usize; 257];
        for (key, _) in &keyed {
            starts[((key >> shift) & 0xff) as usize + 1] += 1;
        }
        for digit in 0..256 {
            starts[digit + 1] += starts[digit];
        }
        for item in &keyed {
            let digit = ((item.0 >> shift) & 0xff) as usize;
            if let Some(slot) = spare.get_mut(starts[digit]) {
                *slot = *item;
            }
            starts[digit] += 1;
        }
        std::mem::swap(&mut keyed, &mut spare);
    }
    keyed
}

#[derive(Clone, Copy)]
enum Role {
    Start,
    End,
    Inner,
}

impl<'s, T: ReadTransaction> Space<'s, '_, T> {
    fn sole_group(&self) -> Option<&'s Group> {
        let search = self.search;
        match search.segments.as_slice() {
            [Segment::Group(group)] if group.hops.len() == 1 => Some(group),
            _ => None,
        }
    }

    fn sole_hop(&self) -> Option<&'s Step> {
        self.sole_group().and_then(|group| group.hops.first())
    }

    fn fits(
        &self,
        known: &mut IdMap<NodeId, bool>,
        node: NodeId,
        role: Role,
    ) -> Result<bool, Error> {
        let (Some(group), Some(hop)) = (self.sole_group(), self.sole_hop()) else {
            return Ok(false);
        };
        match role {
            Role::Start => self.node_ok(node, &group.entry),
            Role::End => Ok(self.node_ok(node, &hop.node)? && self.node_ok(node, &group.exit)?),
            Role::Inner => {
                if trivial(&hop.node) && trivial(&group.entry) {
                    return Ok(true);
                }
                if let Some(fits) = known.get(&node) {
                    return Ok(*fits);
                }
                let fits = self.node_ok(node, &hop.node)? && self.node_ok(node, &group.entry)?;
                known.insert(node, fits);
                Ok(fits)
            }
        }
    }

    fn edge_ok(
        &mut self,
        from: NodeId,
        to: NodeId,
        relationship: RelationshipId,
    ) -> Result<bool, Error> {
        let (Some(group), Some(hop)) = (self.sole_group(), self.sole_hop()) else {
            return Ok(false);
        };
        let pattern = &hop.relationship;
        if pattern.bound && self.row.get(pattern.slot) != Some(&Datum::Relationship(relationship)) {
            return Ok(false);
        }
        if !self
            .env
            .relationship_matches(relationship, pattern, &self.row)?
        {
            return Ok(false);
        }
        if group.filter.is_none() {
            return Ok(true);
        }
        Ok(self
            .after(Point::Boundary(0, 0), (&[from, to], &[relationship]))?
            .is_some())
    }

    fn neighbours(
        &self,
        node: NodeId,
        backward: bool,
        found: &mut Vec<Adjacent>,
    ) -> Result<(), Error> {
        found.clear();
        let Some(hop) = self.sole_hop() else {
            return Ok(());
        };
        let read = self.env.hop_read(&hop.relationship)?;
        let read = if backward { read.reversed() } else { read };
        self.env.hop_into(node, &hop.relationship, read, found)
    }

    fn single_hop(
        &mut self,
        start: NodeId,
        targets: Option<&[NodeId]>,
        selector: Selector,
    ) -> Result<Option<Vec<Found>>, Error> {
        let Some(group) = self.sole_group() else {
            return Ok(None);
        };
        let every = match selector {
            Selector::Paths(1) => false,
            Selector::Groups(1) => true,
            _ => return Ok(None),
        };
        if group.min > 1 || self.search.filter.is_some() {
            return Ok(None);
        }
        let (min, max) = (group.min, group.max);
        let both_ways = self
            .sole_group()
            .and_then(|group| group.hops.first())
            .is_some_and(|hop| {
                matches!(
                    hop.relationship.direction,
                    crate::gql::ast::Direction::Any
                        | crate::gql::ast::Direction::IncomingOrOutgoing
                )
            });
        let mut known = IdMap::default();
        let mut kept = Vec::new();
        if wanted(targets, start) {
            if min == 0 {
                if self.node_ok(start, &group.exit)? {
                    kept.push(Found {
                        trail: PathIds {
                            nodes: vec![start],
                            relationships: Vec::new(),
                        },
                        counts: Vec::new(),
                    });
                }
            } else {
                match self.search.mode {
                    PathMode::Acyclic => {}
                    PathMode::Trail if both_ways => {
                        kept.extend(self.closed_trails(&mut known, start, max, every)?);
                    }
                    PathMode::Walk | PathMode::Simple | PathMode::Trail => {
                        kept.extend(self.forward(
                            &mut known,
                            start,
                            Some(&[]),
                            true,
                            max,
                            every,
                        )?);
                    }
                }
            }
        }
        if max == Some(0) || !self.fits(&mut known, start, Role::Start)? {
            return Ok(Some(kept));
        }
        let others: Option<Vec<NodeId>> = targets.map(|targets| {
            targets
                .iter()
                .copied()
                .filter(|node| *node != start)
                .collect()
        });
        match others.as_deref() {
            Some([]) => {}
            Some([end]) => kept.extend(self.both_ends(&mut known, start, *end, max, every)?),
            others => kept.extend(self.forward(&mut known, start, others, false, max, every)?),
        }
        Ok(Some(kept))
    }

    fn forward(
        &mut self,
        known: &mut IdMap<NodeId, bool>,
        start: NodeId,
        targets: Option<&[NodeId]>,
        closing: bool,
        max: Option<u64>,
        every: bool,
    ) -> Result<Vec<Found>, Error> {
        let mut kept = Vec::new();
        if !self.fits(known, start, Role::Start)? {
            return Ok(kept);
        }
        if closing && !self.fits(known, start, Role::End)? {
            return Ok(kept);
        }
        let mut tree = Tree::new(start);
        let mut buffer = Vec::new();
        if targets.is_none()
            && let (Some(group), Some(hop)) = (self.sole_group(), self.sole_hop())
        {
            self.learn_members(&group.exit)?;
            self.learn_members(&hop.node)?;
            self.learn_members(&group.entry)?;
        }
        let mut layer = vec![0usize];
        let mut depth = 0usize;
        let mut remaining = targets.map_or(usize::MAX, <[NodeId]>::len);
        let mut closings: Vec<(usize, RelationshipId)> = Vec::new();
        while !layer.is_empty() && max.is_none_or(|max| (depth as u64) < max) {
            let mut next = Vec::new();
            for &at in &layer {
                let Some(node) = tree.reached.get(at).map(|reached| reached.node) else {
                    continue;
                };
                self.neighbours(node, false, &mut buffer)?;
                for adjacent in buffer.iter().copied() {
                    let other = adjacent.other;
                    if other == start {
                        if closing
                            && (every || closings.is_empty())
                            && self.edge_ok(node, other, adjacent.relationship)?
                        {
                            closings.push((at, adjacent.relationship));
                        }
                        continue;
                    }
                    if closing {
                        if tree
                            .layer_of(other)
                            .is_some_and(|layer| layer != depth + 1 || !every)
                        {
                            continue;
                        }
                        if !self.fits(known, other, Role::Inner)?
                            || !self.edge_ok(node, other, adjacent.relationship)?
                        {
                            continue;
                        }
                        if tree.reach(other, depth + 1, at, adjacent.relationship, every) {
                            next.push(tree.reached.len() - 1);
                        }
                        continue;
                    }
                    let seen = tree.layer_of(other);
                    if seen.is_some_and(|layer| layer != depth + 1 || !every) {
                        continue;
                    }
                    let target = wanted(targets, other);
                    let inner = self.fits(known, other, Role::Inner)?;
                    let end = target && self.fits(known, other, Role::End)?;
                    if !(inner || end) || !self.edge_ok(node, other, adjacent.relationship)? {
                        continue;
                    }
                    if tree.reach(other, depth + 1, at, adjacent.relationship, every) {
                        let index = tree.reached.len() - 1;
                        if inner {
                            next.push(index);
                        }
                        if end {
                            remaining = remaining.saturating_sub(1);
                        }
                    }
                }
            }
            depth += 1;
            if closing {
                if !closings.is_empty() {
                    for (at, relationship) in &closings {
                        for (mut nodes, mut relationships) in tree.paths(*at, every) {
                            nodes.push(start);
                            relationships.push(*relationship);
                            kept.push(Found {
                                trail: PathIds {
                                    nodes,
                                    relationships,
                                },
                                counts: Vec::new(),
                            });
                        }
                    }
                    return Ok(kept);
                }
            } else if remaining == 0 {
                break;
            }
            tree.sort_layer(&mut next);
            layer = next;
        }
        if closing {
            return Ok(kept);
        }
        for index in 1..tree.reached.len() {
            let Some(node) = tree.reached.get(index).map(|reached| reached.node) else {
                continue;
            };
            if !wanted(targets, node) || !self.fits(known, node, Role::End)? {
                continue;
            }
            for (nodes, relationships) in tree.paths(index, every) {
                kept.push(Found {
                    trail: PathIds {
                        nodes,
                        relationships,
                    },
                    counts: Vec::new(),
                });
            }
        }
        Ok(kept)
    }

    fn closed_trails(
        &mut self,
        known: &mut IdMap<NodeId, bool>,
        start: NodeId,
        max: Option<u64>,
        every: bool,
    ) -> Result<Vec<Found>, Error> {
        let mut best: Option<usize> = None;
        let mut kept: Vec<Found> = Vec::new();
        if !self.fits(known, start, Role::Start)? || !self.fits(known, start, Role::End)? {
            return Ok(kept);
        }
        let mut buffer = Vec::new();
        self.neighbours(start, false, &mut buffer)?;
        for first in buffer {
            if !self.edge_ok(start, first.other, first.relationship)? {
                continue;
            }
            let mut found = Vec::new();
            if first.other == start {
                found.push(Found {
                    trail: PathIds {
                        nodes: vec![start, start],
                        relationships: vec![first.relationship],
                    },
                    counts: Vec::new(),
                });
            } else if self.fits(known, first.other, Role::Inner)? {
                let rest = max.map(|max| max.saturating_sub(1));
                for tail in
                    self.avoiding(known, first.other, start, first.relationship, rest, every)?
                {
                    let mut nodes = vec![start];
                    nodes.extend(tail.trail.nodes);
                    let mut relationships = vec![first.relationship];
                    relationships.extend(tail.trail.relationships);
                    found.push(Found {
                        trail: PathIds {
                            nodes,
                            relationships,
                        },
                        counts: Vec::new(),
                    });
                }
            }
            let Some(length) = found.first().map(|found| found.trail.relationships.len()) else {
                continue;
            };
            match best {
                Some(best) if length > best => continue,
                Some(best) if length == best => {
                    if every {
                        kept.extend(found);
                    }
                }
                _ => {
                    best = Some(length);
                    kept = found;
                }
            }
        }
        if !every {
            kept.truncate(1);
        }
        Ok(kept)
    }

    fn avoiding(
        &mut self,
        known: &mut IdMap<NodeId, bool>,
        from: NodeId,
        to: NodeId,
        banned: RelationshipId,
        max: Option<u64>,
        every: bool,
    ) -> Result<Vec<Found>, Error> {
        let mut tree = Tree::new(from);
        let mut buffer = Vec::new();
        let mut layer = vec![0usize];
        let mut depth = 0usize;
        let mut hits = Vec::new();
        while !layer.is_empty() && hits.is_empty() && max.is_none_or(|max| (depth as u64) < max) {
            let mut next = Vec::new();
            for &at in &layer {
                let Some(node) = tree.reached.get(at).map(|reached| reached.node) else {
                    continue;
                };
                self.neighbours(node, false, &mut buffer)?;
                for adjacent in buffer.iter().copied() {
                    let other = adjacent.other;
                    if adjacent.relationship == banned {
                        continue;
                    }
                    if tree
                        .layer_of(other)
                        .is_some_and(|layer| layer != depth + 1 || !every)
                    {
                        continue;
                    }
                    let fits = other == to || self.fits(known, other, Role::Inner)?;
                    if !fits || !self.edge_ok(node, other, adjacent.relationship)? {
                        continue;
                    }
                    if tree.reach(other, depth + 1, at, adjacent.relationship, every) {
                        let index = tree.reached.len() - 1;
                        if other == to {
                            hits.push(index);
                        } else {
                            next.push(index);
                        }
                    }
                }
            }
            depth += 1;
            tree.sort_layer(&mut next);
            layer = next;
        }
        let mut kept = Vec::new();
        for index in hits {
            for (nodes, relationships) in tree.paths(index, every) {
                kept.push(Found {
                    trail: PathIds {
                        nodes,
                        relationships,
                    },
                    counts: Vec::new(),
                });
            }
        }
        Ok(kept)
    }

    fn both_ends(
        &mut self,
        known: &mut IdMap<NodeId, bool>,
        start: NodeId,
        end: NodeId,
        max: Option<u64>,
        every: bool,
    ) -> Result<Vec<Found>, Error> {
        let mut kept = Vec::new();
        if !self.fits(known, end, Role::End)? {
            return Ok(kept);
        }
        let mut sides = [Tree::new(start), Tree::new(end)];
        let mut buffer = Vec::new();
        let mut layers = [vec![0usize], vec![0usize]];
        let mut depths = [0usize, 0usize];
        loop {
            if layers[0].is_empty() || layers[1].is_empty() {
                return Ok(kept);
            }
            if max.is_some_and(|max| (depths[0] + depths[1]) as u64 >= max) {
                return Ok(kept);
            }
            let side = usize::from(layers[1].len() < layers[0].len());
            let backward = side == 1;
            let (root, other_root) = if backward { (end, start) } else { (start, end) };
            let depth = depths[side];
            let mut next = Vec::new();
            let current = std::mem::take(&mut layers[side]);
            for &at in &current {
                let Some(node) = sides[side].reached.get(at).map(|reached| reached.node) else {
                    continue;
                };
                self.neighbours(node, backward, &mut buffer)?;
                for adjacent in buffer.iter().copied() {
                    let other = adjacent.other;
                    if other == root {
                        continue;
                    }
                    if sides[side]
                        .layer_of(other)
                        .is_some_and(|layer| layer != depth + 1 || !every)
                    {
                        continue;
                    }
                    let fits = other == other_root || self.fits(known, other, Role::Inner)?;
                    let (from, to) = if backward {
                        (other, node)
                    } else {
                        (node, other)
                    };
                    if !fits || !self.edge_ok(from, to, adjacent.relationship)? {
                        continue;
                    }
                    if sides[side].reach(other, depth + 1, at, adjacent.relationship, every) {
                        next.push(sides[side].reached.len() - 1);
                    }
                }
            }
            depths[side] += 1;
            let opposite = 1 - side;
            let meeting: Vec<usize> = next
                .iter()
                .copied()
                .filter(|index| {
                    sides[side].reached.get(*index).is_some_and(|reached| {
                        sides[opposite].layer_of(reached.node) == Some(depths[opposite])
                    })
                })
                .collect();
            if !meeting.is_empty() {
                for index in meeting {
                    let Some(node) = sides[side].reached.get(index).map(|reached| reached.node)
                    else {
                        continue;
                    };
                    let Some(&across) = sides[opposite].at.get(&node) else {
                        continue;
                    };
                    let (forward, back) = if backward {
                        (sides[0].paths(across, every), sides[1].paths(index, every))
                    } else {
                        (sides[0].paths(index, every), sides[1].paths(across, every))
                    };
                    for (head_nodes, head_relationships) in &forward {
                        for (tail_nodes, tail_relationships) in &back {
                            let mut nodes = head_nodes.clone();
                            nodes.extend(tail_nodes.iter().rev().skip(1));
                            let mut relationships = head_relationships.clone();
                            relationships.extend(tail_relationships.iter().rev());
                            kept.push(Found {
                                trail: PathIds {
                                    nodes,
                                    relationships,
                                },
                                counts: Vec::new(),
                            });
                            if !every {
                                return Ok(kept);
                            }
                        }
                    }
                }
                return Ok(kept);
            }
            sides[side].sort_layer(&mut next);
            layers[side] = next;
        }
    }
}
