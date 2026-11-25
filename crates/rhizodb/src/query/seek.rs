use crate::error::Error;
use crate::gql::ast::BinaryOp;
use crate::gql::ast::ComparisonOp;
use crate::gql::ast::Direction;
use crate::gql::ast::LabelExpr;
use crate::graph::Bound;
use crate::graph::Compared;
use crate::graph::KeyRange;
use crate::graph::Label;
use crate::graph::NodeId;
use crate::graph::PropertyKey;
use crate::graph::RelationshipId;
use crate::graph::RelationshipType;
use crate::graph::Target;
use crate::graph::Token;
use crate::graph::compared;
use crate::storage::ReadTransaction;
use crate::value::PropertyValue;

use super::datum::Datum;
use super::eval::Env;
use super::eval::Row;
use super::functions::Function;
use super::plan::Expr;
use super::plan::PatternPart;
use super::plan::RelationshipPattern;
use super::plan::Seek;
use super::plan::SeekElement;
use super::plan::SeekKey;
use super::plan::SeekTest;
use super::plan::Slot;
use super::temporals::Moment;
use super::temporals::TemporalFunction;

const TWO_TO_63: f64 = 9_223_372_036_854_775_808.0;

/// The seeks of `part`, from its property maps and from the conjuncts of `filter`, in the
/// order they are tried: ID lookups, then equality, then ranges. `bound_later` holds the slots
/// the part and the parts after it bind, which a seek value must not read.
pub(crate) fn derive(part: &PatternPart, filter: Option<&Expr>, bound_later: &[Slot]) -> Vec<Seek> {
    if part.start.bound {
        return Vec::new();
    }
    let first = part
        .steps
        .first()
        .map(|step| &step.relationship)
        .filter(|relationship| reaches_start(relationship));
    let mut found = Vec::new();
    let mut from_map = |element: SeekElement, properties: Option<&Expr>| {
        if let Some(Expr::Map(entries)) = properties {
            for (at, (key, value)) in entries.iter().enumerate() {
                let last = !entries[at + 1..].iter().any(|(other, _)| other == key);
                if last && independent(value, bound_later) {
                    found.push((
                        element,
                        SeekKey::Property(key.clone()),
                        SeekTest::Equal(value.clone()),
                        true,
                    ));
                }
            }
        }
    };
    from_map(SeekElement::Start, part.start.properties.as_ref());
    if let Some(relationship) = first {
        from_map(
            SeekElement::FirstRelationship,
            relationship.properties.as_ref(),
        );
    }
    let single = first.filter(|relationship| relationship.length.is_none());
    let element_of = |slot: Slot| {
        if slot == part.start.slot {
            Some(SeekElement::Start)
        } else if single.is_some_and(|relationship| relationship.slot == slot) {
            Some(SeekElement::FirstRelationship)
        } else {
            None
        }
    };
    let value_of = |expr: &Expr| match expr {
        Expr::Property(target, key) => match target.as_ref() {
            Expr::Slot(slot) => Some((element_of(*slot)?, SeekKey::Property(key.clone()))),
            _ => None,
        },
        Expr::Call(Function::Id, args) => match args.as_slice() {
            [Expr::Slot(slot)] => Some((element_of(*slot)?, SeekKey::Id)),
            _ => None,
        },
        _ => None,
    };
    let mut conjuncts = Vec::new();
    if let Some(filter) = filter {
        collect_conjuncts(filter, &mut conjuncts);
    }
    for conjunct in conjuncts {
        match conjunct {
            Expr::Comparison(first, rest) => {
                let mut left = first.as_ref();
                for (op, right) in rest {
                    if let Some((element, key)) = value_of(left)
                        && independent(right, bound_later)
                        && let Some(test) = test(*op, right)
                    {
                        found.push((element, key, test, false));
                    } else if let Some((element, key)) = value_of(right)
                        && independent(left, bound_later)
                        && let Some(test) = test(mirror(*op), left)
                    {
                        found.push((element, key, test, false));
                    }
                    left = right;
                }
            }
            Expr::Binary(BinaryOp::In, needle, list) => {
                if let Some((element, key)) = value_of(needle)
                    && independent(list, bound_later)
                {
                    found.push((element, key, SeekTest::In(list.as_ref().clone()), false));
                }
            }
            _ => {}
        }
    }
    let mut seeks: Vec<Seek> = Vec::new();
    for (element, key, test, map_entry) in found {
        match seeks
            .iter_mut()
            .find(|seek| seek.element == element && seek.key == key)
        {
            Some(seek) => seek.tests.push(test),
            None => seeks.push(Seek {
                element,
                key,
                tests: vec![test],
                map_entry,
            }),
        }
    }
    seeks.retain(|seek| seek.key != SeekKey::Id || seek.tests.iter().any(is_point));
    seeks.sort_by_key(|seek| (seek.key != SeekKey::Id, !seek.tests.iter().any(is_point)));
    seeks
}

/// The value of the one `id()` equality of `filter` that names the start node of `part`, when
/// the ID lookup of the start node reads that equality alone, and `filter` with that equality
/// read as true. `None` when the start node has no such lookup.
pub(crate) fn start_by_id(
    part: &PatternPart,
    filter: Option<&Expr>,
    bound_later: &[Slot],
) -> Option<(Expr, Option<Expr>)> {
    let [first, ..] = part.seeks.as_slice() else {
        return None;
    };
    if first.element != SeekElement::Start
        || first.key != SeekKey::Id
        || !matches!(first.tests.as_slice(), [SeekTest::Equal(_)])
    {
        return None;
    }
    let names_start = |expr: &Expr| match expr {
        Expr::Call(Function::Id, args) => {
            matches!(args.as_slice(), [Expr::Slot(slot)] if *slot == part.start.slot)
        }
        _ => false,
    };
    let filter = filter?;
    let mut conjuncts = Vec::new();
    collect_conjuncts(filter, &mut conjuncts);
    let mut served = conjuncts.into_iter().filter_map(|conjunct| match conjunct {
        Expr::Comparison(left, rest) => match rest.as_slice() {
            [(ComparisonOp::Equal, right)]
                if names_start(left) && independent(right, bound_later) =>
            {
                Some((conjunct, right))
            }
            [(ComparisonOp::Equal, right)]
                if names_start(right) && independent(left, bound_later) =>
            {
                Some((conjunct, left.as_ref()))
            }
            _ => None,
        },
        _ => None,
    });
    let (conjunct, value) = served.next()?;
    if served.next().is_some() {
        return None;
    }
    let rest = (!std::ptr::eq(conjunct, filter)).then(|| read_as_true(filter, conjunct));
    Some((value.clone(), rest))
}

fn read_as_true(expr: &Expr, conjunct: &Expr) -> Expr {
    if std::ptr::eq(expr, conjunct) {
        return Expr::Constant(Datum::Boolean(true));
    }
    match expr {
        Expr::Binary(BinaryOp::And, left, right) => Expr::Binary(
            BinaryOp::And,
            Box::new(read_as_true(left, conjunct)),
            Box::new(read_as_true(right, conjunct)),
        ),
        other => other.clone(),
    }
}

fn reaches_start(relationship: &RelationshipPattern) -> bool {
    !relationship.bound
        && relationship.direction != Direction::Undirected
        && relationship.length.is_none_or(|hops| hops.min >= 1)
}

fn collect_conjuncts<'a>(expr: &'a Expr, out: &mut Vec<&'a Expr>) {
    match expr {
        Expr::Binary(BinaryOp::And, left, right) => {
            collect_conjuncts(left, out);
            collect_conjuncts(right, out);
        }
        other => out.push(other),
    }
}

fn test(op: ComparisonOp, value: &Expr) -> Option<SeekTest> {
    let bound = match op {
        ComparisonOp::Equal => return Some(SeekTest::Equal(value.clone())),
        ComparisonOp::NotEqual => return None,
        ComparisonOp::Less => Bound::Less,
        ComparisonOp::LessEqual => Bound::LessEqual,
        ComparisonOp::Greater => Bound::Greater,
        ComparisonOp::GreaterEqual => Bound::GreaterEqual,
    };
    Some(SeekTest::Compare(bound, value.clone()))
}

fn mirror(op: ComparisonOp) -> ComparisonOp {
    match op {
        ComparisonOp::Less => ComparisonOp::Greater,
        ComparisonOp::LessEqual => ComparisonOp::GreaterEqual,
        ComparisonOp::Greater => ComparisonOp::Less,
        ComparisonOp::GreaterEqual => ComparisonOp::LessEqual,
        other => other,
    }
}

fn is_point(test: &SeekTest) -> bool {
    matches!(test, SeekTest::Equal(_) | SeekTest::In(_))
}

fn independent(expr: &Expr, bound_later: &[Slot]) -> bool {
    let all = |items: &[Expr]| items.iter().all(|item| independent(item, bound_later));
    let maybe = |item: Option<&Expr>| item.is_none_or(|item| independent(item, bound_later));
    match expr {
        Expr::Constant(_) | Expr::Parameter(_) => true,
        Expr::Slot(slot) => !bound_later.contains(slot),
        Expr::Property(inner, _)
        | Expr::Not(inner)
        | Expr::Negate(inner)
        | Expr::UnaryPlus(inner)
        | Expr::HasLabels(inner, _)
        | Expr::IsNull { operand: inner, .. } => independent(inner, bound_later),
        Expr::Index(left, right) | Expr::Binary(_, left, right) => {
            independent(left, bound_later) && independent(right, bound_later)
        }
        Expr::Slice(list, from, to) => {
            independent(list, bound_later) && maybe(from.as_deref()) && maybe(to.as_deref())
        }
        Expr::Comparison(first, rest) => {
            independent(first, bound_later)
                && rest.iter().all(|(_, item)| independent(item, bound_later))
        }
        Expr::List(items) => all(items),
        Expr::Map(entries) => entries
            .iter()
            .all(|(_, value)| independent(value, bound_later)),
        Expr::Call(function, args) => deterministic(*function) && all(args),
        Expr::Case(case) => {
            maybe(case.scrutinee.as_ref())
                && maybe(case.otherwise.as_ref())
                && case.branches.iter().all(|(when, then)| {
                    independent(when, bound_later) && independent(then, bound_later)
                })
        }
        Expr::ListComprehension(comprehension) => {
            independent(&comprehension.source, bound_later)
                && maybe(comprehension.filter.as_ref())
                && maybe(comprehension.projection.as_ref())
        }
        Expr::Quantified(quantified) => {
            independent(&quantified.source, bound_later)
                && independent(&quantified.predicate, bound_later)
        }
        Expr::Reduce(reduce) => {
            independent(&reduce.init, bound_later)
                && independent(&reduce.source, bound_later)
                && independent(&reduce.body, bound_later)
        }
        Expr::Admit(..)
        | Expr::Exists(_)
        | Expr::CountMatches(_)
        | Expr::Subquery(..)
        | Expr::PatternComprehension(..) => false,
    }
}

fn deterministic(function: Function) -> bool {
    !matches!(
        function,
        Function::Rand | Function::Temporal(TemporalFunction::Clock(_, Moment::Realtime))
    )
}

type Found = (Vec<u64>, Option<Token<PropertyKey>>);

/// The start nodes a seek found, ascending and each once.
#[derive(Debug)]
pub(crate) struct Sought {
    pub(crate) nodes: Vec<NodeId>,
    /// Whether every node carries the label the start node pattern names, as when it came
    /// from a property index over that label.
    pub(crate) labelled: bool,
    /// The property whose entry in the start node pattern's map holds for every node, as when
    /// an index lookup by that entry's equality found them.
    pub(crate) served: Option<Token<PropertyKey>>,
}

impl<T: ReadTransaction> Env<'_, T> {
    /// The start nodes of `part` that the first seek which applies finds, or `None` when none
    /// applies and the label or the whole graph has to be read. Every match starts at one of
    /// them. A seek value that fails to evaluate leaves the seek out, so the scan raises the
    /// error where it would have.
    pub(crate) fn seek_start(
        &self,
        part: &PatternPart,
        row: &Row,
    ) -> Result<Option<Sought>, Error> {
        for seek in &part.seeks {
            let (elements, served) = match &seek.key {
                SeekKey::Id => (self.id_seek(seek, row), None),
                SeekKey::Property(key) => match self.property_seek(part, seek, key, row)? {
                    Some((elements, served)) => (Some(elements), served),
                    None => (None, None),
                },
            };
            let Some(elements) = elements else {
                continue;
            };
            return Ok(Some(match seek.element {
                SeekElement::Start => Sought {
                    nodes: elements.into_iter().map(NodeId).collect(),
                    labelled: matches!(seek.key, SeekKey::Property(_)),
                    served,
                },
                SeekElement::FirstRelationship => Sought {
                    nodes: self.endpoints(part, elements)?,
                    labelled: false,
                    served: None,
                },
            }));
        }
        Ok(None)
    }

    fn id_seek(&self, seek: &Seek, row: &Row) -> Option<Vec<u64>> {
        seek.tests.iter().find_map(|test| match test {
            SeekTest::Equal(expr) => {
                let value = self.eval(expr, row).ok()?;
                Some(id(&value).into_iter().collect())
            }
            SeekTest::In(expr) => match self.eval(expr, row).ok()? {
                Datum::Null => Some(Vec::new()),
                Datum::List(items) => {
                    let mut ids: Vec<u64> = items.iter().filter_map(id).collect();
                    ids.sort_unstable();
                    ids.dedup();
                    Some(ids)
                }
                _ => None,
            },
            SeekTest::Compare(..) => None,
        })
    }

    fn property_seek(
        &self,
        part: &PatternPart,
        seek: &Seek,
        key: &str,
        row: &Row,
    ) -> Result<Option<Found>, Error> {
        let target = match seek.element {
            SeekElement::Start => {
                let Some(label) = part.start.labels.as_ref().and_then(LabelExpr::required) else {
                    return Ok(None);
                };
                match self.tx.lookup_token::<Label>(&label.0)? {
                    Some(token) => Target::Label(token),
                    None => return Ok(Some((Vec::new(), None))),
                }
            }
            SeekElement::FirstRelationship => {
                let Some(LabelExpr::Name(rel_type)) = part
                    .steps
                    .first()
                    .and_then(|step| step.relationship.labels.as_ref())
                else {
                    return Ok(None);
                };
                match self.tx.lookup_token::<RelationshipType>(&rel_type.0)? {
                    Some(token) => Target::RelationshipType(token),
                    None => return Ok(Some((Vec::new(), None))),
                }
            }
        };
        let Some(property) = self.tx.lookup_token::<PropertyKey>(key)? else {
            return Ok(None);
        };
        let Some(index) = self.property_index(target, property)? else {
            return Ok(None);
        };
        let mut points: Option<Vec<KeyRange>> = None;
        let mut range: Option<KeyRange> = None;
        let mut served = None;
        for (index, test) in seek.tests.iter().enumerate() {
            match test {
                SeekTest::Equal(expr) if points.is_none() => {
                    if let Ok(value) = self.eval(expr, row) {
                        points = Some(equal_to(&value).into_iter().collect());
                        served =
                            (index == 0 && seek.map_entry && seek.element == SeekElement::Start)
                                .then_some(property);
                    }
                }
                SeekTest::In(expr) if points.is_none() => match self.eval(expr, row) {
                    Ok(Datum::Null) => points = Some(Vec::new()),
                    Ok(Datum::List(items)) => {
                        points = Some(items.iter().filter_map(equal_to).collect());
                    }
                    _ => {}
                },
                SeekTest::Compare(bound, expr) => {
                    let Ok(value) = self.eval(expr, row) else {
                        continue;
                    };
                    match compare_to(*bound, &value) {
                        Compared::Range(found) => {
                            match range.map_or(Some(found.clone()), |held| held.intersect(&found)) {
                                Some(narrowed) => range = Some(narrowed),
                                None => return Ok(Some((Vec::new(), None))),
                            }
                        }
                        Compared::Never => return Ok(Some((Vec::new(), None))),
                        Compared::Unserved => {}
                    }
                }
                SeekTest::Equal(_) | SeekTest::In(_) => {}
            }
        }
        let served = served.filter(|_| points.is_some());
        let Some(ranges) = points.or_else(|| range.map(|range| vec![range])) else {
            return Ok(None);
        };
        Ok(Some((self.tx.property_seek(index, &ranges)?, served)))
    }

    fn endpoints(&self, part: &PatternPart, relationships: Vec<u64>) -> Result<Vec<NodeId>, Error> {
        let Some(step) = part.steps.first() else {
            return Ok(Vec::new());
        };
        let (sources, targets) = match step.relationship.direction {
            Direction::Outgoing | Direction::UndirectedOrOutgoing => (true, false),
            Direction::Incoming | Direction::IncomingOrUndirected => (false, true),
            Direction::IncomingOrOutgoing | Direction::Any => (true, true),
            Direction::Undirected => (false, false),
        };
        let mut nodes = Vec::new();
        for relationship in relationships {
            if let Some(record) = self.tx.relationship(RelationshipId(relationship))? {
                if sources {
                    nodes.push(record.source);
                }
                if targets {
                    nodes.push(record.target);
                }
            }
        }
        nodes.sort_unstable();
        nodes.dedup();
        Ok(nodes)
    }
}

/// The element ID that `id()` of an element equal to `value` gives, if any.
pub(crate) fn id(value: &Datum) -> Option<u64> {
    match value {
        Datum::Integer(id) => Some(*id as u64),
        Datum::Float(id) if id.fract() == 0.0 && (-TWO_TO_63..TWO_TO_63).contains(id) => {
            Some(*id as i64 as u64)
        }
        _ => None,
    }
}

fn equal_to(value: &Datum) -> Option<KeyRange> {
    KeyRange::equal_to(&stored(value)?)
}

fn compare_to(bound: Bound, value: &Datum) -> Compared {
    match value {
        Datum::List(_) => Compared::Unserved,
        other => match stored(other) {
            Some(value) => compared(bound, &value),
            None => Compared::Never,
        },
    }
}

fn stored(value: &Datum) -> Option<PropertyValue> {
    Some(match value {
        Datum::Boolean(value) => PropertyValue::Boolean(*value),
        Datum::Integer(value) => PropertyValue::Integer(*value),
        Datum::Float(value) => PropertyValue::Float(*value),
        Datum::String(value) => PropertyValue::String(value.clone()),
        Datum::Bytes(value) => PropertyValue::Bytes(value.clone()),
        Datum::Temporal(value) => PropertyValue::Temporal(value.clone()),
        Datum::List(items) => PropertyValue::List(items.iter().map(stored).collect::<Option<_>>()?),
        Datum::Null | Datum::Map(_) | Datum::Node(_) | Datum::Relationship(_) | Datum::Path(_) => {
            return None;
        }
    })
}

#[cfg(test)]
#[path = "seek.test.rs"]
mod tests;
