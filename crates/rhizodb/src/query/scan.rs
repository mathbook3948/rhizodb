use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::sync::Arc;

use crate::error::Error;
use crate::gql::ast::BinaryOp;
use crate::gql::ast::ComparisonOp;
use crate::gql::ast::LabelExpr;
use crate::gql::ast::SortDirection;
use crate::graph::Cell;
use crate::graph::Label;
use crate::graph::NodeId;
use crate::graph::PropertyKey;
use crate::graph::ScanGroup;
use crate::graph::Target;
use crate::graph::Token;
use crate::storage::ReadTransaction;

use super::aggregate::Groups;
use super::aggregate::Part;
use super::compare::equals;
use super::compare::order;
use super::datum::Datum;
use super::eval::Env;
use super::eval::Row;
use super::eval::and;
use super::eval::comparison;
use super::eval::or;
use super::eval::set;
use super::keys::KeySet;
use super::keys::Probe;
use super::plan::Expr;
use super::plan::Items;
use super::plan::MatchClause;
use super::plan::PatternPart;
use super::plan::Projection;
use super::plan::SeekElement;
use super::plan::SeekKey;
use super::plan::Slot;

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
        if found.optional || row_at_a_time() {
            return None;
        }
        let [part] = found.patterns.as_slice() else {
            return None;
        };
        let start = &part.start;
        if part.path.is_some()
            || !part.steps.is_empty()
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
                Pred::Text(*op, lhs, rhs)
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

    fn text(&self) -> Option<&str> {
        match self {
            Arg::Cell(Cell::Str(text)) => Some(text),
            Arg::Datum(datum) => match datum.as_ref() {
                Datum::String(text) => Some(text),
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
        Cell::Str(value) => Datum::String(value.to_owned()),
        other => Datum::from(other.to_value()?),
    })
}

fn text_comparison(op: ComparisonOp, text: &str, other: &Datum, mirrored: bool) -> Option<bool> {
    match op {
        ComparisonOp::Equal | ComparisonOp::NotEqual => {
            let equal = match other {
                Datum::Null => return None,
                Datum::String(other) => text == other,
                _ => false,
            };
            Some(equal == (op == ComparisonOp::Equal))
        }
        _ => {
            let Datum::String(other) = other else {
                return None;
            };
            let ordering = if mirrored {
                other.as_str().cmp(text)
            } else {
                text.cmp(other)
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
        (Arg::Cell(Cell::Str(text)), Arg::Datum(other)) => text_comparison(op, text, other, false),
        (Arg::Datum(other), Arg::Cell(Cell::Str(text))) => text_comparison(op, text, other, true),
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

    fn holds(&self, pred: &Pred) -> Result<Option<bool>, Error> {
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
                        _ => text.contains(part),
                    }),
                    _ => None,
                }
            }
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
            (Some(Cell::Int(value)), Datum::Integer(fixed)) => Some(ordered(
                op,
                if mirrored {
                    fixed.cmp(&value)
                } else {
                    value.cmp(fixed)
                },
            )),
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
        let ordering = order(x, y);
        let ordering = match direction {
            SortDirection::Ascending => ordering,
            SortDirection::Descending => ordering.reverse(),
        };
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
        let mut groups: Vec<ScanGroup<'_>> = Vec::new();
        for (input, row) in rows.iter().enumerate() {
            let mut held = 0;
            for node in &ids {
                if groups.get(held).is_some_and(|group| !group.holds(*node)) {
                    if rows.len() == 1 {
                        groups.clear();
                    } else {
                        held += 1;
                    }
                }
                if groups.len() == held {
                    groups.push(self.tx.scan_group(*node, tokens)?);
                }
                let Some(group) = groups.get(held) else {
                    continue;
                };
                let at = At {
                    group,
                    node: *node,
                    row,
                };
                if at.keeps(filter)? && !visit(&at, input)? {
                    return Ok(());
                }
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
                let output = outputs
                    .iter()
                    .map(|value| at.arg(value)?.probe())
                    .collect::<Result<Vec<_>, _>>()?;
                if !seen.insert_probe(&output) {
                    return Ok(true);
                }
                if outputs_are_items && !early {
                    values = Some(output.iter().map(Probe::owned).collect());
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
            for value in &sort_values {
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
        Pred::IsNull(value, _) | Pred::In(value, _) => reads(value),
        Pred::Text(_, lhs, rhs) => reads(lhs) || reads(rhs),
        Pred::Not(operand) => pred_reads(operand, column),
        Pred::Logic(_, lhs, rhs) => pred_reads(lhs, column) || pred_reads(rhs, column),
    }
}

#[cfg(test)]
#[path = "scan.test.rs"]
mod tests;
