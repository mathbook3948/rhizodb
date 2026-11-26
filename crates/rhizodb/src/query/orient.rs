use std::collections::HashSet;

use crate::gql::ast::Direction;

use super::plan::NodePattern;
use super::plan::PatternPart;
use super::plan::RelationshipPattern;
use super::plan::Seek;
use super::plan::Slot;
use super::plan::Step;
use super::seek::independent;

/// Picks where a match of `part`, as written, begins. A part whose leftmost node is unbound
/// begins at its leftmost node that the row already binds, if any. Otherwise it keeps its
/// leftmost node and lists in [`PatternPart::others`] the nodes that an ID or index lookup may
/// serve, for the match to choose from when it runs, since only then is it known which indexes
/// exist. `seeks` gives the seeks of a part begun at its start node.
///
/// A part keeps its leftmost start when a property map in it reads an element the part binds,
/// which another order may not have reached yet.
pub(crate) fn orient(part: &mut PatternPart, seeks: impl Fn(&PatternPart) -> Vec<Seek>) {
    let introduced = introduced(part);
    let own: HashSet<Slot> = introduced.iter().copied().collect();
    let movable = std::iter::once(&part.start.properties)
        .chain(
            part.steps
                .iter()
                .flat_map(|step| [&step.node.properties, &step.relationship.properties]),
        )
        .flatten()
        .all(|properties| independent(properties, &introduced));
    let count = part.steps.len() + 1;
    if movable
        && !part.start.bound
        && let Some(at) = (1..count).find(|at| {
            let node = node_at(part, *at);
            node.bound && !own.contains(&node.slot)
        })
    {
        *part = anchored(part, at, &own);
    }
    part.seeks = seeks(part);
    if !movable || part.start.bound {
        return;
    }
    part.others = (1..count)
        .filter(|at| !seeks(&first_hop(part, *at, &own)).is_empty())
        .map(|at| {
            let mut other = anchored(part, at, &own);
            other.seeks = seeks(&other);
            other
        })
        .collect();
}

fn introduced(part: &PatternPart) -> Vec<Slot> {
    let mut own = Vec::new();
    for node in std::iter::once(&part.start).chain(part.steps.iter().map(|step| &step.node)) {
        if !node.bound {
            own.push(node.slot);
        }
    }
    for step in &part.steps {
        if !step.relationship.bound {
            own.push(step.relationship.slot);
        }
    }
    own
}

fn node_at(part: &PatternPart, at: usize) -> &NodePattern {
    match at {
        0 => &part.start,
        at => &part.steps[at - 1].node,
    }
}

fn first_met(node: &NodePattern, own: &HashSet<Slot>) -> NodePattern {
    NodePattern {
        bound: node.bound && !own.contains(&node.slot),
        ..node.clone()
    }
}

fn first_hop(part: &PatternPart, at: usize, own: &HashSet<Slot>) -> PatternPart {
    PatternPart {
        path: None,
        start: first_met(node_at(part, at), own),
        steps: vec![Step {
            relationship: flipped(&part.steps[at - 1].relationship),
            node: node_at(part, at - 1).clone(),
        }],
        left: 1,
        seeks: Vec::new(),
        others: Vec::new(),
    }
}

fn anchored(part: &PatternPart, at: usize, own: &HashSet<Slot>) -> PatternPart {
    let mut seen = HashSet::new();
    let mut walked = |node: &NodePattern| {
        let mut met = first_met(node, own);
        met.bound |= !seen.insert(node.slot);
        met
    };
    let start = walked(node_at(part, at));
    let mut steps = Vec::with_capacity(part.steps.len());
    for index in (0..at).rev() {
        steps.push(Step {
            relationship: flipped(&part.steps[index].relationship),
            node: walked(node_at(part, index)),
        });
    }
    for step in &part.steps[at..] {
        steps.push(Step {
            relationship: step.relationship.clone(),
            node: walked(&step.node),
        });
    }
    PatternPart {
        path: part.path,
        start,
        steps,
        left: at,
        seeks: Vec::new(),
        others: Vec::new(),
    }
}

fn flipped(relationship: &RelationshipPattern) -> RelationshipPattern {
    let direction = match relationship.direction {
        Direction::Outgoing => Direction::Incoming,
        Direction::Incoming => Direction::Outgoing,
        Direction::UndirectedOrOutgoing => Direction::IncomingOrUndirected,
        Direction::IncomingOrUndirected => Direction::UndirectedOrOutgoing,
        same @ (Direction::Undirected | Direction::IncomingOrOutgoing | Direction::Any) => same,
    };
    RelationshipPattern {
        direction,
        ..relationship.clone()
    }
}
