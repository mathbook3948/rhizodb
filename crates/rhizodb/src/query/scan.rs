use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::collections::HashMap;
use std::hash::BuildHasherDefault;
use std::hash::Hasher;
use std::sync::Arc;

use memchr::memmem;

use crate::error::Error;
use crate::gql::ast::BinaryOp;
use crate::gql::ast::ComparisonOp;
use crate::gql::ast::LabelExpr;
use crate::gql::ast::SortDirection;
use crate::graph::Cell;
use crate::graph::Label;
use crate::graph::NodeId;
use crate::graph::PropertyKey;
use crate::graph::Quick;
use crate::graph::ScanGroup;
use crate::graph::Target;
use crate::graph::Text;
use crate::graph::Token;
use crate::storage::ReadTransaction;

use super::aggregate::Groups;
use super::aggregate::Part;
use super::compare::equals;
use super::compare::sort_order;
use super::datum::Datum;
use super::eval::Env;
use super::eval::Row;
use super::eval::and;
use super::eval::comparison;
use super::eval::or;
use super::eval::set;
use super::keys::KeySet;
use super::keys::Probe;
use super::matcher::HopRead;
use super::plan::Expr;
use super::plan::Items;
use super::plan::MatchClause;
use super::plan::PatternPart;
use super::plan::Projection;
use super::plan::SeekElement;
use super::plan::SeekKey;
use super::plan::Slot;
use super::plan::Step;

/// `MATCH (n)` or `MATCH (n:Label)` with no other pattern element: every live node, or every
/// live node with the label, in ascending ID order.
#[derive(Clone, Copy, Debug)]
pub(crate) struct NodeScan<'q> {
    slot: Slot,
    label: Option<&'q str>,
    part: &'q PatternPart,
}

impl<'q> NodeScan<'q> {
    pub(crate) fn of(found: &'q MatchClause) -> Option<Self> {
        Self::start(found).filter(|scan| scan.part.steps.is_empty())
    }

    fn start(found: &'q MatchClause) -> Option<Self> {
        if found.optional || row_at_a_time() {
            return None;
        }
        let [part] = found.patterns.as_slice() else {
            return None;
        };
        let start = &part.start;
        if part.path.is_some()
            || start.bound
            || !matches!(start.properties, None | Some(Expr::Map(_)))
        {
            return None;
        }
        let label = match &start.labels {
            None => None,
            Some(LabelExpr::Name(name)) => Some(name.0.as_str()),
            Some(_) => return None,
        };
        Some(Self {
            slot: start.slot,
            label,
            part,
        })
    }
}

#[derive(Clone, Copy, Debug)]
struct HopScan<'q> {
    scan: NodeScan<'q>,
    step: &'q Step,
}

impl<'q> HopScan<'q> {
    fn of(found: &'q MatchClause) -> Option<Self> {
        let scan = NodeScan::start(found)?;
        let [step] = scan.part.steps.as_slice() else {
            return None;
        };
        let relationship = &step.relationship;
        if relationship.bound
            || relationship.length.is_some()
            || relationship.properties.is_some()
            || step.node.bound
            || step.node.properties.is_some()
        {
            return None;
        }
        Some(Self { scan, step })
    }
}

enum HopValue<'q> {
    Scanned(Value),
    Row(&'q Expr),
}

#[derive(Default)]
struct IdHasher(u64);

impl Hasher for IdHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.write_u64(u64::from(*byte));
        }
    }

    fn write_u64(&mut self, value: u64) {
        self.0 = (self.0.rotate_left(5) ^ value).wrapping_mul(0x517c_c1b7_2722_0a95);
    }

    fn write_usize(&mut self, value: usize) {
        self.write_u64(value as u64);
    }
}

type ById<K, V> = HashMap<K, V, BuildHasherDefault<IdHasher>>;

#[cfg(not(test))]
fn row_at_a_time() -> bool {
    false
}

#[cfg(test)]
pub(crate) use tests::row_at_a_time;

#[derive(Clone, Debug)]
enum Value {
    Column(usize),
    Node,
    Fixed(Datum),
    Row(Slot),
}

#[derive(Clone, Debug)]
enum Pred {
    Fixed(Option<bool>),
    Against {
        column: usize,
        op: ComparisonOp,
        fixed: Datum,
        mirrored: bool,
    },
    Compare(Value, Vec<(ComparisonOp, Value)>),
    IsNull(Value, bool),
    Text(BinaryOp, Value, Value),
    /// `CONTAINS` of a fixed string, with its searcher built once.
    Contains(Value, Box<memmem::Finder<'static>>),
    In(Value, Option<Vec<Datum>>),
    Not(Box<Pred>),
    Logic(BinaryOp, Box<Pred>, Box<Pred>),
}

struct Compiler<'e, 'a, T: ReadTransaction> {
    env: &'e Env<'a, T>,
    slot: Slot,
    tokens: Vec<Option<Token<PropertyKey>>>,
    names: Vec<String>,
    aliases: Vec<(Slot, Value)>,
    forbidden: Vec<Slot>,
}

impl<'e, 'a, T: ReadTransaction> Compiler<'e, 'a, T> {
    fn new(env: &'e Env<'a, T>, slot: Slot) -> Self {
        Self {
            env,
            slot,
            tokens: Vec::new(),
            names: Vec::new(),
            aliases: Vec::new(),
            forbidden: Vec::new(),
        }
    }

    fn column(&mut self, key: &str) -> Result<usize, Error> {
        if let Some(index) = self.names.iter().position(|name| name == key) {
            return Ok(index);
        }
        self.tokens.push(self.env.property_key(key)?);
        self.names.push(key.to_owned());
        Ok(self.names.len() - 1)
    }

    fn value(&mut self, expr: &Expr) -> Result<Option<Value>, Error> {
        Ok(Some(match expr {
            Expr::Constant(value) => Value::Fixed(value.clone()),
            Expr::Parameter(index) => Value::Fixed(
                self.env
                    .parameters
                    .get(*index)
                    .cloned()
                    .unwrap_or(Datum::Null),
            ),
            Expr::Slot(slot) => {
                if let Some((_, value)) = self.aliases.iter().find(|(held, _)| held == slot) {
                    value.clone()
                } else if *slot == self.slot {
                    Value::Node
                } else if self.forbidden.contains(slot) {
                    return Ok(None);
                } else {
                    Value::Row(*slot)
                }
            }
            Expr::Property(base, key) => match base.as_ref() {
                Expr::Slot(slot) if *slot == self.slot && !self.aliased(*slot) => {
                    Value::Column(self.column(key)?)
                }
                _ => return Ok(None),
            },
            _ => return Ok(None),
        }))
    }

    fn hop_value<'q>(&mut self, expr: &'q Expr) -> Result<HopValue<'q>, Error> {
        Ok(match self.value(expr)? {
            Some(value) => HopValue::Scanned(value),
            None => HopValue::Row(expr),
        })
    }

    fn aliased(&self, slot: Slot) -> bool {
        self.aliases.iter().any(|(held, _)| *held == slot)
    }

    fn values<'x>(
        &mut self,
        exprs: impl IntoIterator<Item = &'x Expr>,
    ) -> Result<Option<Vec<Value>>, Error> {
        let mut values = Vec::new();
        for expr in exprs {
            let Some(value) = self.value(expr)? else {
                return Ok(None);
            };
            values.push(value);
        }
        Ok(Some(values))
    }

    #[inline(never)]
    fn comparison(
        &mut self,
        first: &Expr,
        rest: &[(ComparisonOp, Expr)],
    ) -> Result<Option<Pred>, Error> {
        let Some(first) = self.value(first)? else {
            return Ok(None);
        };
        if let [(op, operand)] = rest {
            let Some(operand) = self.value(operand)? else {
                return Ok(None);
            };
            return Ok(Some(match (first, operand) {
                (Value::Column(column), Value::Fixed(fixed)) => Pred::Against {
                    column,
                    op: *op,
                    fixed,
                    mirrored: false,
                },
                (Value::Fixed(fixed), Value::Column(column)) => Pred::Against {
                    column,
                    op: *op,
                    fixed,
                    mirrored: true,
                },
                (first, operand) => Pred::Compare(first, vec![(*op, operand)]),
            }));
        }
        let mut operands = Vec::with_capacity(rest.len());
        for (op, operand) in rest {
            let Some(operand) = self.value(operand)? else {
                return Ok(None);
            };
            operands.push((*op, operand));
        }
        Ok(Some(Pred::Compare(first, operands)))
    }

    fn pred(&mut self, expr: &Expr) -> Result<Option<Pred>, Error> {
        Ok(Some(match expr {
            Expr::Constant(Datum::Boolean(value)) => Pred::Fixed(Some(*value)),
            Expr::Constant(Datum::Null) => Pred::Fixed(None),
            Expr::Comparison(first, rest) => return self.comparison(first, rest),
            Expr::IsNull { operand, negated } => match self.value(operand)? {
                Some(operand) => Pred::IsNull(operand, *negated),
                None => return Ok(None),
            },
            Expr::Not(operand) => match self.pred(operand)? {
                Some(operand) => Pred::Not(Box::new(operand)),
                None => return Ok(None),
            },
            Expr::Binary(op @ (BinaryOp::And | BinaryOp::Or | BinaryOp::Xor), lhs, rhs) => {
                let (Some(lhs), Some(rhs)) = (self.pred(lhs)?, self.pred(rhs)?) else {
                    return Ok(None);
                };
                Pred::Logic(*op, Box::new(lhs), Box::new(rhs))
            }
            Expr::Binary(
                op @ (BinaryOp::StartsWith | BinaryOp::EndsWith | BinaryOp::Contains),
                lhs,
                rhs,
            ) => {
                let (Some(lhs), Some(rhs)) = (self.value(lhs)?, self.value(rhs)?) else {
                    return Ok(None);
                };
                match (op, rhs) {
                    (BinaryOp::Contains, Value::Fixed(Datum::String(part))) => {
                        Pred::Contains(lhs, Box::new(memmem::Finder::new(&part).into_owned()))
                    }
                    (op, rhs) => Pred::Text(*op, lhs, rhs),
                }
            }
            Expr::Binary(BinaryOp::In, needle, list) => {
                let Some(needle) = self.value(needle)? else {
                    return Ok(None);
                };
                match self.value(list)? {
                    Some(Value::Fixed(Datum::List(items))) => {
                        Pred::In(needle, Some(Arc::unwrap_or_clone(items)))
                    }
                    Some(Value::Fixed(Datum::Null)) => Pred::In(needle, None),
                    _ => return Ok(None),
                }
            }
            _ => return Ok(None),
        }))
    }
}

#[derive(Debug)]
enum Arg<'c> {
    Cell(Cell<'c>),
    Datum(Cow<'c, Datum>),
}

impl<'c> Arg<'c> {
    fn datum(&self) -> Result<Cow<'_, Datum>, Error> {
        Ok(match self {
            Arg::Cell(cell) => Cow::Owned(cell_datum(*cell)?),
            Arg::Datum(datum) => Cow::Borrowed(datum.as_ref()),
        })
    }

    fn probe(self) -> Result<Probe<'c>, Error> {
        Ok(match self {
            Arg::Cell(Cell::Str(text)) => Probe::Text(text),
            Arg::Cell(cell) => Probe::Value(Cow::Owned(cell_datum(cell)?)),
            Arg::Datum(datum) => Probe::Value(datum),
        })
    }

    fn text(&self) -> Option<&[u8]> {
        match self {
            Arg::Cell(Cell::Str(text)) => Some(text.bytes()),
            Arg::Datum(datum) => match datum.as_ref() {
                Datum::String(text) => Some(text.as_bytes()),
                _ => None,
            },
            Arg::Cell(_) => None,
        }
    }

    fn is_null(&self) -> bool {
        match self {
            Arg::Cell(_) => false,
            Arg::Datum(datum) => datum.is_null(),
        }
    }
}

fn cell_datum(cell: Cell<'_>) -> Result<Datum, Error> {
    Ok(match cell {
        Cell::Bool(value) => Datum::Boolean(value),
        Cell::Int(value) => Datum::Integer(value),
        Cell::Float(value) => Datum::Float(value),
        Cell::Str(value) => Datum::String(value.to_str()?.to_owned()),
        other => Datum::from(other.to_value()?),
    })
}

fn text_comparison(
    op: ComparisonOp,
    text: Text<'_>,
    other: &Datum,
    mirrored: bool,
) -> Option<bool> {
    match op {
        ComparisonOp::Equal | ComparisonOp::NotEqual => {
            let equal = match other {
                Datum::Null => return None,
                Datum::String(other) => text.bytes() == other.as_bytes(),
                _ => false,
            };
            Some(equal == (op == ComparisonOp::Equal))
        }
        _ => {
            let Datum::String(other) = other else {
                return None;
            };
            let ordering = if mirrored {
                other.as_bytes().cmp(text.bytes())
            } else {
                text.bytes().cmp(other.as_bytes())
            };
            Some(ordered(op, ordering))
        }
    }
}

fn ordered(op: ComparisonOp, ordering: Ordering) -> bool {
    match op {
        ComparisonOp::Equal => ordering.is_eq(),
        ComparisonOp::NotEqual => ordering.is_ne(),
        ComparisonOp::Less => ordering.is_lt(),
        ComparisonOp::LessEqual => ordering.is_le(),
        ComparisonOp::Greater => ordering.is_gt(),
        ComparisonOp::GreaterEqual => ordering.is_ge(),
    }
}

fn int_order(op: ComparisonOp, stored: i64, fixed: i64, mirrored: bool) -> bool {
    ordered(
        op,
        if mirrored {
            fixed.cmp(&stored)
        } else {
            stored.cmp(&fixed)
        },
    )
}

fn compare_args(op: ComparisonOp, left: &Arg<'_>, right: &Arg<'_>) -> Result<Option<bool>, Error> {
    if let (Arg::Cell(Cell::Int(left)), Arg::Datum(right)) = (left, right)
        && let Datum::Integer(right) = right.as_ref()
    {
        return Ok(Some(ordered(op, left.cmp(right))));
    }
    if let (Arg::Datum(left), Arg::Cell(Cell::Int(right))) = (left, right)
        && let Datum::Integer(left) = left.as_ref()
    {
        return Ok(Some(ordered(op, left.cmp(right))));
    }
    Ok(match (left, right) {
        (Arg::Cell(Cell::Str(text)), Arg::Datum(other)) => text_comparison(op, *text, other, false),
        (Arg::Datum(other), Arg::Cell(Cell::Str(text))) => text_comparison(op, *text, other, true),
        _ => comparison(op, &*left.datum()?, &*right.datum()?),
    })
}

fn member(needle: &Arg<'_>, items: Option<&[Datum]>) -> Result<Option<bool>, Error> {
    let Some(items) = items else {
        return Ok(None);
    };
    let needle_datum;
    let text = match needle {
        Arg::Cell(Cell::Str(text)) => Some(*text),
        _ => None,
    };
    let datum = match text {
        Some(_) => None,
        None => {
            needle_datum = needle.datum()?;
            Some(needle_datum.as_ref())
        }
    };
    let mut unknown = false;
    for item in items {
        let equal = match (text, datum) {
            (Some(text), _) => text_comparison(ComparisonOp::Equal, text, item, false),
            (None, Some(datum)) => equals(datum, item),
            (None, None) => None,
        };
        match equal {
            Some(true) => return Ok(Some(true)),
            None => unknown = true,
            Some(false) => {}
        }
    }
    Ok(if unknown { None } else { Some(false) })
}

struct At<'g, 'r> {
    group: &'g ScanGroup<'g>,
    node: NodeId,
    row: &'r Row,
}

impl At<'_, '_> {
    fn arg<'v>(&'v self, value: &'v Value) -> Result<Arg<'v>, Error> {
        Ok(match value {
            Value::Column(column) => match self.group.cell(self.node, *column)? {
                Some(cell) => Arg::Cell(cell),
                None => Arg::Datum(Cow::Owned(Datum::Null)),
            },
            Value::Node => Arg::Datum(Cow::Owned(Datum::Node(self.node))),
            Value::Fixed(value) => Arg::Datum(Cow::Borrowed(value)),
            Value::Row(slot) => match self.row.get(*slot) {
                Some(value) => Arg::Datum(Cow::Borrowed(value)),
                None => Arg::Datum(Cow::Owned(Datum::Null)),
            },
        })
    }

    fn datum(&self, value: &Value) -> Result<Datum, Error> {
        if let Value::Column(column) = value {
            match self.group.quick(self.node, *column) {
                Quick::Absent => return Ok(Datum::Null),
                Quick::Int(value) => return Ok(Datum::Integer(value)),
                Quick::Unknown => {}
            }
            return match self.group.cell(self.node, *column)? {
                Some(cell) => cell_datum(cell),
                None => Ok(Datum::Null),
            };
        }
        Ok(match self.arg(value)? {
            Arg::Cell(cell) => cell_datum(cell)?,
            Arg::Datum(datum) => datum.into_owned(),
        })
    }

    #[inline]
    fn holds(&self, pred: &Pred) -> Result<Option<bool>, Error> {
        match self.quick(pred) {
            Some(result) => Ok(result),
            None => self.evaluate(pred),
        }
    }

    #[inline]
    fn quick(&self, pred: &Pred) -> Option<Option<bool>> {
        match pred {
            Pred::Against {
                column,
                op,
                fixed,
                mirrored,
            } => match (self.group.quick(self.node, *column), fixed) {
                (Quick::Int(value), Datum::Integer(fixed)) => {
                    Some(Some(int_order(*op, value, *fixed, *mirrored)))
                }
                (Quick::Absent, fixed) if *mirrored => Some(comparison(*op, fixed, &Datum::Null)),
                (Quick::Absent, fixed) => Some(comparison(*op, &Datum::Null, fixed)),
                _ => None,
            },
            Pred::IsNull(Value::Column(column), negated) => {
                match self.group.quick(self.node, *column) {
                    Quick::Absent => Some(Some(!*negated)),
                    Quick::Int(_) => Some(Some(*negated)),
                    Quick::Unknown => None,
                }
            }
            _ => None,
        }
    }

    #[inline(never)]
    fn evaluate(&self, pred: &Pred) -> Result<Option<bool>, Error> {
        Ok(match pred {
            Pred::Fixed(value) => *value,
            Pred::Against {
                column,
                op,
                fixed,
                mirrored,
            } => self.against(*column, *op, fixed, *mirrored)?,
            Pred::Compare(first, rest) => {
                let mut left = self.arg(first)?;
                let mut result = Some(true);
                for (op, operand) in rest {
                    let right = self.arg(operand)?;
                    result = and(result, compare_args(*op, &left, &right)?);
                    left = right;
                }
                result
            }
            Pred::IsNull(Value::Column(column), negated) => {
                Some(self.group.cell(self.node, *column)?.is_none() != *negated)
            }
            Pred::IsNull(operand, negated) => Some(self.arg(operand)?.is_null() != *negated),
            Pred::Text(op, lhs, rhs) => {
                let (lhs, rhs) = (self.arg(lhs)?, self.arg(rhs)?);
                match (lhs.text(), rhs.text()) {
                    (Some(text), Some(part)) => Some(match op {
                        BinaryOp::StartsWith => text.starts_with(part),
                        BinaryOp::EndsWith => text.ends_with(part),
                        _ => memmem::find(text, part).is_some(),
                    }),
                    _ => None,
                }
            }
            Pred::Contains(text, finder) => self
                .arg(text)?
                .text()
                .map(|text| finder.find(text).is_some()),
            Pred::In(needle, items) => member(&self.arg(needle)?, items.as_deref())?,
            Pred::Not(operand) => self.holds(operand)?.map(|value| !value),
            Pred::Logic(op, lhs, rhs) => {
                let (lhs, rhs) = (self.holds(lhs)?, self.holds(rhs)?);
                match op {
                    BinaryOp::And => and(lhs, rhs),
                    BinaryOp::Or => or(lhs, rhs),
                    _ => lhs.zip(rhs).map(|(a, b)| a != b),
                }
            }
        })
    }

    #[inline(never)]
    fn against(
        &self,
        column: usize,
        op: ComparisonOp,
        fixed: &Datum,
        mirrored: bool,
    ) -> Result<Option<bool>, Error> {
        Ok(match (self.group.cell(self.node, column)?, fixed) {
            (Some(Cell::Int(value)), Datum::Integer(fixed)) => {
                Some(int_order(op, value, *fixed, mirrored))
            }
            (cell, fixed) => {
                let stored = match cell {
                    Some(cell) => Arg::Cell(cell),
                    None => Arg::Datum(Cow::Owned(Datum::Null)),
                };
                let fixed = Arg::Datum(Cow::Borrowed(fixed));
                if mirrored {
                    compare_args(op, &fixed, &stored)?
                } else {
                    compare_args(op, &stored, &fixed)?
                }
            }
        })
    }

    fn keeps(&self, filter: Option<&Pred>) -> Result<bool, Error> {
        match filter {
            Some(filter) => Ok(self.holds(filter)? == Some(true)),
            None => Ok(true),
        }
    }
}

struct Ranked {
    keys: Vec<Datum>,
    position: usize,
    node: NodeId,
    input: usize,
    items: Option<Vec<Datum>>,
}

fn rank(a: &Ranked, b: &Ranked, directions: &[SortDirection]) -> Ordering {
    rank_keys(&a.keys, a.position, b, directions)
}

fn rank_keys(
    keys: &[Datum],
    position: usize,
    b: &Ranked,
    directions: &[SortDirection],
) -> Ordering {
    for ((x, y), direction) in keys.iter().zip(&b.keys).zip(directions) {
        let ordering = sort_order(*direction, x, y);
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    position.cmp(&b.position)
}

struct Heaped<'d>(Ranked, &'d [SortDirection]);

impl PartialEq for Heaped<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Heaped<'_> {}

impl PartialOrd for Heaped<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Heaped<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        rank(&self.0, &other.0, self.1)
    }
}

enum Chosen<'d> {
    All(Vec<Ranked>),
    Top(BinaryHeap<Heaped<'d>>, usize),
}

impl<'d> Chosen<'d> {
    fn new(kept: Option<usize>, ordered: bool) -> Self {
        match kept {
            Some(kept) if ordered => Chosen::Top(BinaryHeap::new(), kept),
            _ => Chosen::All(Vec::new()),
        }
    }

    fn offer(
        &mut self,
        keys: &mut Vec<Datum>,
        (position, node, input): (usize, NodeId, usize),
        items: Option<Vec<Datum>>,
        directions: &'d [SortDirection],
    ) {
        let ranked = |keys: &mut Vec<Datum>, items| Ranked {
            keys: std::mem::take(keys),
            position,
            node,
            input,
            items,
        };
        match self {
            Chosen::All(all) => all.push(ranked(keys, items)),
            Chosen::Top(heap, kept) => {
                if heap.len() < *kept {
                    heap.push(Heaped(ranked(keys, items), directions));
                } else if let Some(mut worst) = heap.peek_mut()
                    && rank_keys(keys, position, &worst.0, directions) == Ordering::Less
                {
                    std::mem::swap(&mut worst.0.keys, keys);
                    worst.0.position = position;
                    worst.0.node = node;
                    worst.0.input = input;
                    worst.0.items = items;
                }
            }
        }
        keys.clear();
    }

    fn worst(&self) -> Option<&Ranked> {
        match self {
            Chosen::Top(heap, kept) if heap.len() >= *kept => heap.peek().map(|worst| &worst.0),
            _ => None,
        }
    }

    fn len(&self) -> usize {
        match self {
            Chosen::All(all) => all.len(),
            Chosen::Top(heap, _) => heap.len(),
        }
    }

    fn sorted(self, directions: &[SortDirection], ordered: bool) -> Vec<Ranked> {
        let mut chosen = match self {
            Chosen::All(all) => all,
            Chosen::Top(heap, _) => heap.into_iter().map(|heaped| heaped.0).collect(),
        };
        if ordered {
            chosen.sort_unstable_by(|a, b| rank(a, b, directions));
        }
        chosen
    }
}

impl<T: ReadTransaction> Env<'_, T> {
    fn seek_possible(&self, part: &PatternPart) -> Result<bool, Error> {
        for seek in &part.seeks {
            let SeekKey::Property(key) = &seek.key else {
                return Ok(true);
            };
            if seek.element != SeekElement::Start {
                return Ok(true);
            }
            let Some(label) = part.start.labels.as_ref().and_then(LabelExpr::required) else {
                continue;
            };
            let Some(label) = self.tx.lookup_token::<Label>(&label.0)? else {
                continue;
            };
            let Some(property) = self.tx.lookup_token::<PropertyKey>(key)? else {
                continue;
            };
            if self
                .property_index(Target::Label(label), property)?
                .is_some()
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The node scan that answers `found`, unless the clause has another form or an index or ID
    /// lookup may serve its pattern.
    pub(crate) fn node_scan<'q>(
        &self,
        found: &'q MatchClause,
    ) -> Result<Option<NodeScan<'q>>, Error> {
        match NodeScan::of(found) {
            Some(scan) if !self.seek_possible(scan.part)? => Ok(Some(scan)),
            _ => Ok(None),
        }
    }

    fn scanned_ids(&self, scan: &NodeScan<'_>) -> Result<Vec<NodeId>, Error> {
        Ok(match scan.label {
            Some(label) => match self.label_token(label)? {
                Some(token) => self.tx.node_ids_with_label(token)?,
                None => Vec::new(),
            },
            None => self.tx.node_ids()?,
        })
    }

    fn scan(
        &self,
        scan: &NodeScan<'_>,
        rows: &[Row],
        tokens: &[Option<Token<PropertyKey>>],
        filter: Option<&Pred>,
        visit: &mut dyn FnMut(&At<'_, '_>, usize) -> Result<bool, Error>,
    ) -> Result<(), Error> {
        #[cfg(test)]
        tests::scanned();
        if rows.is_empty() {
            return Ok(());
        }
        let ids = self.scanned_ids(scan)?;
        let mut groups: Vec<(ScanGroup<'_>, usize)> = Vec::new();
        for (input, row) in rows.iter().enumerate() {
            let mut start = 0;
            let mut held = 0;
            while let Some(first) = ids.get(start) {
                if groups.len() == held {
                    if rows.len() == 1 {
                        groups.clear();
                        held = 0;
                    }
                    let group = self.tx.scan_group(*first, tokens)?;
                    let end = start + ids[start..].partition_point(|node| group.holds(*node));
                    groups.push((group, end));
                }
                let Some((group, end)) = groups.get(held) else {
                    break;
                };
                for node in &ids[start..*end] {
                    let at = At {
                        group,
                        node: *node,
                        row,
                    };
                    if at.keeps(filter)? && !visit(&at, input)? {
                        return Ok(());
                    }
                }
                start = *end;
                held += 1;
            }
        }
        Ok(())
    }

    fn scan_filter(
        &self,
        compiler: &mut Compiler<'_, '_, T>,
        scan: &NodeScan<'_>,
        found: &MatchClause,
    ) -> Result<Option<Option<Pred>>, Error> {
        let mut conjuncts = Vec::new();
        if let Some(Expr::Map(entries)) = &scan.part.start.properties {
            for (at, (key, value)) in entries.iter().enumerate() {
                if entries[at + 1..].iter().any(|(other, _)| other == key) {
                    continue;
                }
                let Some(value) = compiler.value(value)? else {
                    return Ok(None);
                };
                let column = Value::Column(compiler.column(key)?);
                conjuncts.push(Pred::Compare(column, vec![(ComparisonOp::Equal, value)]));
            }
        }
        if let Some(filter) = &found.filter {
            let Some(filter) = compiler.pred(filter)? else {
                return Ok(None);
            };
            conjuncts.push(filter);
        }
        Ok(Some(conjuncts.into_iter().reduce(|all, next| {
            Pred::Logic(BinaryOp::And, Box::new(all), Box::new(next))
        })))
    }

    /// `MATCH` over `scan`, reading only the columns its filter tests. `None` when the filter has
    /// another form.
    pub(crate) fn scan_match(
        &self,
        scan: NodeScan<'_>,
        found: &MatchClause,
        rows: &[Row],
    ) -> Result<Option<Vec<Row>>, Error> {
        let mut compiler = Compiler::new(self, scan.slot);
        let Some(filter) = self.scan_filter(&mut compiler, &scan, found)? else {
            return Ok(None);
        };
        let mut out = Vec::new();
        self.scan(
            &scan,
            rows,
            &compiler.tokens,
            filter.as_ref(),
            &mut |at, _| {
                let mut row = at.row.clone();
                set(&mut row, scan.slot, Datum::Node(at.node));
                out.push(row);
                Ok(true)
            },
        )?;
        Ok(Some(out))
    }

    /// `MATCH` over `scan` fed straight into the projection after it, which reads only the columns
    /// it needs from each node and builds a row only for each node it keeps. `None` when the filter
    /// or the projection has another form.
    pub(crate) fn scan_project(
        &self,
        scan: NodeScan<'_>,
        found: &MatchClause,
        projection: &Projection,
        rows: &[Row],
    ) -> Result<Option<Vec<Row>>, Error> {
        let mut compiler = Compiler::new(self, scan.slot);
        let Some(filter) = self.scan_filter(&mut compiler, &scan, found)? else {
            return Ok(None);
        };
        match &projection.items {
            Items::Plain(items) => self.scan_plain(scan, compiler, filter, projection, items, rows),
            Items::Aggregate {
                keys,
                aggregates,
                finals,
            } => {
                let Some(key_values) = compiler.values(keys.iter().map(|(expr, _)| expr))? else {
                    return Ok(None);
                };
                let mut arguments = Vec::with_capacity(aggregates.len());
                let mut extras = Vec::with_capacity(aggregates.len());
                for (aggregate, _) in aggregates {
                    let argument = match &aggregate.argument {
                        Some(argument) => match compiler.value(argument)? {
                            Some(value) => Some(value),
                            None => return Ok(None),
                        },
                        None => None,
                    };
                    let extra = match &aggregate.extra {
                        Some(extra) => match compiler.value(extra)? {
                            Some(value) => Some(value),
                            None => return Ok(None),
                        },
                        None => None,
                    };
                    arguments.push(argument);
                    extras.push(extra);
                }
                let mut groups = Groups::new(keys, aggregates);
                let counts_only = keys.is_empty()
                    && aggregates
                        .iter()
                        .all(|(aggregate, _)| aggregate.counts_rows_binding(scan.slot));
                let tokens = compiler.tokens;
                if counts_only {
                    let mut count = 0usize;
                    self.scan(&scan, rows, &tokens, filter.as_ref(), &mut |_, _| {
                        count += 1;
                        Ok(true)
                    })?;
                    groups.count_rows(count);
                } else {
                    self.scan(&scan, rows, &tokens, filter.as_ref(), &mut |at, _| {
                        groups.add_with(|part, _| {
                            let value = match part {
                                Part::Key(index) => key_values.get(index),
                                Part::Argument(index) => {
                                    arguments.get(index).and_then(Option::as_ref)
                                }
                                Part::Extra(index) => extras.get(index).and_then(Option::as_ref),
                            };
                            match value {
                                Some(value) => at.datum(value),
                                None => Ok(Datum::Null),
                            }
                        })?;
                        Ok(true)
                    })?;
                }
                let rows = groups.finish(self, finals)?;
                Ok(Some(self.arrange(projection, rows)?))
            }
        }
    }

    /// `MATCH (a)-[r]->(b)`, one hop of fixed length with no property map on `r` or `b`, fed
    /// straight into the aggregating projection after it. `a` is read as a node scan, which
    /// answers the filter and what the projection reads of `a`, and the rest is evaluated on each
    /// matched row. A node `b` is tested against its pattern once, and when the keys read only
    /// `b` and values fixed for the input row, each `b` finds its group once. `None` when the
    /// clause or the projection has another form, or an index or ID lookup may serve `a`.
    pub(crate) fn hop_aggregate(
        &self,
        found: &MatchClause,
        projection: &Projection,
        rows: &[Row],
    ) -> Result<Option<Vec<Row>>, Error> {
        let Some(hop) = HopScan::of(found) else {
            return Ok(None);
        };
        let Items::Aggregate {
            keys,
            aggregates,
            finals,
        } = &projection.items
        else {
            return Ok(None);
        };
        if self.check.is_some() || self.seek_possible(hop.scan.part)? {
            return Ok(None);
        }
        let start = hop.scan.slot;
        let relationship = &hop.step.relationship;
        let far = hop.step.node.slot;
        let mut compiler = Compiler::new(self, start);
        compiler.forbidden = vec![relationship.slot, far];
        let Some(filter) = self.scan_filter(&mut compiler, &hop.scan, found)? else {
            return Ok(None);
        };
        compiler.forbidden.clear();
        let mut key_values = Vec::with_capacity(keys.len());
        for (expr, _) in keys {
            key_values.push(compiler.hop_value(expr)?);
        }
        let mut arguments = Vec::with_capacity(aggregates.len());
        let mut extras = Vec::with_capacity(aggregates.len());
        for (aggregate, _) in aggregates {
            arguments.push(
                aggregate
                    .argument
                    .as_ref()
                    .map(|expr| compiler.hop_value(expr))
                    .transpose()?,
            );
            extras.push(
                aggregate
                    .extra
                    .as_ref()
                    .map(|expr| compiler.hop_value(expr))
                    .transpose()?,
            );
        }
        let by_far = key_values.iter().all(|value| match value {
            HopValue::Scanned(Value::Fixed(_)) => true,
            HopValue::Scanned(Value::Row(slot)) => *slot != relationship.slot,
            HopValue::Scanned(Value::Column(_) | Value::Node) => false,
            HopValue::Row(Expr::Property(base, _)) => {
                matches!(base.as_ref(), Expr::Slot(slot) if *slot == far)
            }
            HopValue::Row(_) => false,
        });
        let tokens = compiler.tokens;
        let mut groups = Groups::new(keys, aggregates);
        let mut positions: ById<(usize, NodeId), usize> = ById::default();
        let mut fits: ById<NodeId, bool> = ById::default();
        let mut row = Vec::new();
        let mut row_of = None;
        let mut adjacent = Vec::new();
        let read = self.hop_read(relationship)?;
        if matches!(read, HopRead::Nothing) {
            let rows = groups.finish(self, finals)?;
            return Ok(Some(self.arrange(projection, rows)?));
        }
        self.scan(
            &hop.scan,
            rows,
            &tokens,
            filter.as_ref(),
            &mut |at, input| {
                adjacent.clear();
                self.hop_into(at.node, relationship, read, &mut adjacent)?;
                if adjacent.is_empty() {
                    return Ok(true);
                }
                if row_of != Some(input) {
                    row.clone_from(at.row);
                    row_of = Some(input);
                }
                set(&mut row, start, Datum::Node(at.node));
                for adjacent in &adjacent {
                    let other = adjacent.other;
                    let fit = match fits.get(&other) {
                        Some(fit) => *fit,
                        None => {
                            let fit = self.node_fits(other, &hop.step.node, &row)?;
                            fits.insert(other, fit);
                            fit
                        }
                    };
                    if !fit {
                        continue;
                    }
                    set(
                        &mut row,
                        relationship.slot,
                        Datum::Relationship(adjacent.relationship),
                    );
                    set(&mut row, far, Datum::Node(other));
                    let here = At {
                        group: at.group,
                        node: at.node,
                        row: &row,
                    };
                    let value = |value: &HopValue<'_>| match value {
                        HopValue::Scanned(value) => here.datum(value),
                        HopValue::Row(expr) => self.eval(expr, &row),
                    };
                    let position = match by_far.then(|| positions.get(&(input, other))).flatten() {
                        Some(position) => *position,
                        None => {
                            let values = key_values
                                .iter()
                                .map(value)
                                .collect::<Result<Vec<_>, _>>()?;
                            let position = groups.group(values);
                            if by_far {
                                positions.insert((input, other), position);
                            }
                            position
                        }
                    };
                    groups.accumulate(position, |part, _| {
                        let read = match part {
                            Part::Argument(index) => arguments.get(index).and_then(Option::as_ref),
                            Part::Extra(index) => extras.get(index).and_then(Option::as_ref),
                            Part::Key(index) => key_values.get(index),
                        };
                        match read {
                            Some(read) => value(read),
                            None => Ok(Datum::Null),
                        }
                    })?;
                }
                Ok(true)
            },
        )?;
        let rows = groups.finish(self, finals)?;
        Ok(Some(self.arrange(projection, rows)?))
    }

    fn scan_plain(
        &self,
        scan: NodeScan<'_>,
        mut compiler: Compiler<'_, '_, T>,
        filter: Option<Pred>,
        projection: &Projection,
        items: &[(Expr, Slot)],
        rows: &[Row],
    ) -> Result<Option<Vec<Row>>, Error> {
        compiler.forbidden = items.iter().map(|(_, slot)| *slot).collect();
        let Some(item_values) = compiler.values(items.iter().map(|(expr, _)| expr))? else {
            return Ok(None);
        };
        compiler.forbidden.clear();
        compiler.aliases = items
            .iter()
            .map(|(_, slot)| *slot)
            .zip(item_values.iter().cloned())
            .collect();
        let Some(sort_values) =
            compiler.values(projection.order_by.iter().map(|(expr, _)| expr))?
        else {
            return Ok(None);
        };
        let directions: Vec<SortDirection> = projection
            .order_by
            .iter()
            .map(|(_, direction)| *direction)
            .collect();
        let outputs = if projection.distinct {
            let slots: Vec<Expr> = projection
                .outputs
                .iter()
                .map(|slot| Expr::Slot(*slot))
                .collect();
            match compiler.values(&slots)? {
                Some(outputs) => Some(outputs),
                None => return Ok(None),
            }
        } else {
            None
        };
        let empty = vec![Datum::Null; self.slots];
        let skip = match &projection.skip {
            Some(skip) => self.count(skip, &empty, "SKIP")?,
            None => 0,
        };
        let limit = match &projection.limit {
            Some(limit) => Some(self.count(limit, &empty, "LIMIT")?),
            None => None,
        };
        let kept = limit.map(|limit| skip.saturating_add(limit));
        let ordered = !directions.is_empty();
        let early = kept.is_some();
        let tokens: Vec<Option<Token<PropertyKey>>> = compiler
            .tokens
            .iter()
            .enumerate()
            .map(|(index, token)| {
                if !early || is_read(filter.as_ref(), &sort_values, outputs.as_deref(), index) {
                    *token
                } else {
                    None
                }
            })
            .collect();
        let outputs_are_items = projection
            .outputs
            .iter()
            .eq(items.iter().map(|(_, slot)| slot));
        let mut seen = KeySet::default();
        let mut chosen = Chosen::new(kept, ordered);
        let mut keys = Vec::with_capacity(sort_values.len());
        let mut position = 0usize;
        self.scan(&scan, rows, &tokens, filter.as_ref(), &mut |at, input| {
            let mut values = None;
            if let Some(outputs) = &outputs {
                let one;
                let many;
                let output = match outputs.as_slice() {
                    [value] => {
                        one = at.arg(value)?.probe()?;
                        std::slice::from_ref(&one)
                    }
                    _ => {
                        many = outputs
                            .iter()
                            .map(|value| at.arg(value)?.probe())
                            .collect::<Result<Vec<_>, _>>()?;
                        many.as_slice()
                    }
                };
                if !seen.insert_probe(output)? {
                    return Ok(true);
                }
                if outputs_are_items && !early {
                    values = Some(output.iter().map(Probe::owned).collect::<Result<_, _>>()?);
                }
            }
            if values.is_none() && !early {
                values = Some(
                    item_values
                        .iter()
                        .map(|value| at.datum(value))
                        .collect::<Result<Vec<_>, _>>()?,
                );
            }
            if let Some(worst) = chosen.worst() {
                let mut ahead = false;
                for ((value, held), direction) in
                    sort_values.iter().zip(&worst.keys).zip(&directions)
                {
                    let key = at.datum(value)?;
                    let ordering = sort_order(*direction, &key, held);
                    keys.push(key);
                    if ordering != Ordering::Equal {
                        ahead = ordering == Ordering::Less;
                        break;
                    }
                }
                if !ahead {
                    keys.clear();
                    position += 1;
                    return Ok(true);
                }
            }
            for value in &sort_values[keys.len()..] {
                keys.push(at.datum(value)?);
            }
            chosen.offer(&mut keys, (position, at.node, input), values, &directions);
            position += 1;
            Ok(ordered || kept.is_none_or(|kept| chosen.len() < kept))
        })?;
        let mut out = Vec::new();
        for ranked in chosen
            .sorted(&directions, ordered)
            .into_iter()
            .skip(skip)
            .take(limit.unwrap_or(usize::MAX))
        {
            let Some(input) = rows.get(ranked.input) else {
                continue;
            };
            let mut row = input.clone();
            set(&mut row, scan.slot, Datum::Node(ranked.node));
            match ranked.items {
                Some(values) => {
                    for ((_, slot), value) in items.iter().zip(values) {
                        set(&mut row, *slot, value);
                    }
                }
                None => {
                    for (expr, slot) in items {
                        let value = self.eval(expr, &row)?;
                        set(&mut row, *slot, value);
                    }
                }
            }
            out.push(row);
        }
        if let Some(filter) = &projection.filter {
            let mut kept = Vec::new();
            for row in out {
                if self.predicate(filter, &row)? {
                    kept.push(row);
                }
            }
            out = kept;
        }
        Ok(Some(out))
    }
}

fn is_read(
    filter: Option<&Pred>,
    sorts: &[Value],
    outputs: Option<&[Value]>,
    column: usize,
) -> bool {
    filter.is_some_and(|filter| pred_reads(filter, column))
        || sorts.iter().any(|value| reads(value, column))
        || outputs.is_some_and(|outputs| outputs.iter().any(|value| reads(value, column)))
}

fn reads(value: &Value, column: usize) -> bool {
    matches!(value, Value::Column(held) if *held == column)
}

fn pred_reads(pred: &Pred, column: usize) -> bool {
    let reads = |value: &Value| reads(value, column);
    match pred {
        Pred::Fixed(_) => false,
        Pred::Against { column: held, .. } => *held == column,
        Pred::Compare(first, rest) => reads(first) || rest.iter().any(|(_, value)| reads(value)),
        Pred::IsNull(value, _) | Pred::In(value, _) | Pred::Contains(value, _) => reads(value),
        Pred::Text(_, lhs, rhs) => reads(lhs) || reads(rhs),
        Pred::Not(operand) => pred_reads(operand, column),
        Pred::Logic(_, lhs, rhs) => pred_reads(lhs, column) || pred_reads(rhs, column),
    }
}

#[cfg(test)]
#[path = "scan.test.rs"]
mod tests;
