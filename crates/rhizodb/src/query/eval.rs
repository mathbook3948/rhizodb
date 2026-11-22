use std::collections::BTreeMap;

use crate::error::Error;
use crate::error::ErrorKind;
use crate::gql::ast::BinaryOp;
use crate::gql::ast::ComparisonOp;
use crate::gql::ast::Quantifier;
use crate::graph::GraphTx;
use crate::graph::Label;
use crate::graph::NodeId;
use crate::graph::NodeRecord;
use crate::graph::PropertyKey;
use crate::graph::RelationshipId;
use crate::graph::RelationshipRecord;
use crate::storage::ReadTransaction;

use super::compare::Comparison;
use super::compare::compare;
use super::compare::equals;
use super::datum::Datum;
use super::plan::Expr;
use super::plan::Slot;

/// One row: a value for every slot of the query.
pub(crate) type Row = Vec<Datum>;

/// Evaluates expressions and matches patterns against one transaction's view of the graph.
pub(crate) struct Env<'a, T> {
    pub(crate) tx: &'a GraphTx<T>,
    /// How many slots each row of the query has.
    pub(crate) slots: usize,
}

pub(crate) fn type_error(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::TypeMismatch, message)
}

fn deleted() -> Error {
    Error::new(
        ErrorKind::DeletedEntityAccess,
        "the element was deleted earlier in the query",
    )
}

/// Reads a value as a boolean for three-valued logic. `None` is null.
pub(crate) fn truth(value: &Datum) -> Result<Option<bool>, Error> {
    match value {
        Datum::Boolean(value) => Ok(Some(*value)),
        Datum::Null => Ok(None),
        other => Err(type_error(format!(
            "expected a boolean, got a {}",
            other.type_name()
        ))),
    }
}

fn from_truth(value: Option<bool>) -> Datum {
    value.map_or(Datum::Null, Datum::Boolean)
}

fn overflow() -> Error {
    Error::new(ErrorKind::IntegerOverflow, "integer arithmetic overflowed")
}

impl<T: ReadTransaction> Env<'_, T> {
    pub(crate) fn node(&self, id: NodeId) -> Result<NodeRecord, Error> {
        self.tx.node(id)?.ok_or_else(deleted)
    }

    pub(crate) fn relationship(&self, id: RelationshipId) -> Result<RelationshipRecord, Error> {
        self.tx.relationship(id)?.ok_or_else(deleted)
    }

    pub(crate) fn label_names(&self, record: &NodeRecord) -> Result<Vec<String>, Error> {
        record
            .labels
            .iter()
            .map(|label| Ok(self.tx.token_name(*label)?))
            .collect()
    }

    pub(crate) fn property_map(
        &self,
        properties: &crate::graph::Properties,
    ) -> Result<BTreeMap<String, Datum>, Error> {
        properties
            .iter()
            .map(|(key, value)| Ok((self.tx.token_name(*key)?, Datum::from(value.clone()))))
            .collect()
    }

    fn element_property(&self, value: &Datum, key: &str) -> Result<Option<Datum>, Error> {
        let properties = match value {
            Datum::Node(id) => self.node(*id)?.properties,
            Datum::Relationship(id) => self.relationship(*id)?.properties,
            _ => return Ok(None),
        };
        let Some(token) = self.tx.lookup_token::<PropertyKey>(key)? else {
            return Ok(Some(Datum::Null));
        };
        Ok(Some(
            properties
                .get(&token)
                .cloned()
                .map_or(Datum::Null, Datum::from),
        ))
    }

    pub(crate) fn property(&self, value: &Datum, key: &str) -> Result<Datum, Error> {
        match value {
            Datum::Null => Ok(Datum::Null),
            Datum::Map(entries) => Ok(entries.get(key).cloned().unwrap_or(Datum::Null)),
            Datum::Node(_) | Datum::Relationship(_) => {
                Ok(self.element_property(value, key)?.unwrap_or(Datum::Null))
            }
            other => Err(Error::new(
                ErrorKind::PropertyAccessOnNonEntity,
                format!("cannot read property `{key}` of a {}", other.type_name()),
            )),
        }
    }

    pub(crate) fn has_labels(&self, node: NodeId, labels: &[String]) -> Result<bool, Error> {
        let record = self.node(node)?;
        for label in labels {
            match self.tx.lookup_token::<Label>(label)? {
                Some(token) if record.labels.contains(&token) => {}
                _ => return Ok(false),
            }
        }
        Ok(true)
    }

    pub(crate) fn predicate(&self, expr: &Expr, row: &Row) -> Result<bool, Error> {
        Ok(truth(&self.eval(expr, row)?)? == Some(true))
    }

    pub(crate) fn eval(&self, expr: &Expr, row: &Row) -> Result<Datum, Error> {
        match expr {
            Expr::Constant(value) | Expr::Parameter(value) => Ok(value.clone()),
            Expr::Slot(slot) => Ok(row.get(*slot).cloned().unwrap_or(Datum::Null)),
            Expr::List(items) => items
                .iter()
                .map(|item| self.eval(item, row))
                .collect::<Result<_, _>>()
                .map(Datum::List),
            Expr::Map(entries) => entries
                .iter()
                .map(|(key, value)| Ok((key.clone(), self.eval(value, row)?)))
                .collect::<Result<_, Error>>()
                .map(Datum::Map),
            Expr::Property(base, key) => self.property(&self.eval(base, row)?, key),
            Expr::Index(base, index) => self.index(self.eval(base, row)?, self.eval(index, row)?),
            Expr::Slice(base, from, to) => {
                let base = self.eval(base, row)?;
                let from = from.as_ref().map(|from| self.eval(from, row)).transpose()?;
                let to = to.as_ref().map(|to| self.eval(to, row)).transpose()?;
                slice(base, from, to)
            }
            Expr::HasLabels(operand, labels) => match self.eval(operand, row)? {
                Datum::Null => Ok(Datum::Null),
                Datum::Node(id) => Ok(Datum::Boolean(self.has_labels(id, labels)?)),
                other => Err(type_error(format!(
                    "labels can only be tested on a node, not a {}",
                    other.type_name()
                ))),
            },
            Expr::Not(operand) => Ok(from_truth(truth(&self.eval(operand, row)?)?.map(|b| !b))),
            Expr::Negate(operand) => match self.eval(operand, row)? {
                Datum::Null => Ok(Datum::Null),
                Datum::Integer(value) => {
                    value.checked_neg().map(Datum::Integer).ok_or_else(overflow)
                }
                Datum::Float(value) => Ok(Datum::Float(-value)),
                other => Err(type_error(format!("cannot negate a {}", other.type_name()))),
            },
            Expr::UnaryPlus(operand) => match self.eval(operand, row)? {
                value @ (Datum::Null | Datum::Integer(_) | Datum::Float(_)) => Ok(value),
                other => Err(type_error(format!(
                    "unary plus needs a number, not a {}",
                    other.type_name()
                ))),
            },
            Expr::Binary(op, lhs, rhs) => {
                let lhs = self.eval(lhs, row)?;
                let rhs = self.eval(rhs, row)?;
                binary(*op, lhs, rhs)
            }
            Expr::Comparison(first, rest) => {
                let mut left = self.eval(first, row)?;
                let mut result = Some(true);
                for (op, operand) in rest {
                    let right = self.eval(operand, row)?;
                    let step = comparison(*op, &left, &right);
                    result = and(result, step);
                    left = right;
                }
                Ok(from_truth(result))
            }
            Expr::IsNull { operand, negated } => Ok(Datum::Boolean(
                self.eval(operand, row)?.is_null() != *negated,
            )),
            Expr::Call(function, args) => {
                let args = args
                    .iter()
                    .map(|arg| self.eval(arg, row))
                    .collect::<Result<Vec<_>, _>>()?;
                self.call(*function, args)
            }
            Expr::Case(case) => {
                let scrutinee = case
                    .scrutinee
                    .as_ref()
                    .map(|scrutinee| self.eval(scrutinee, row))
                    .transpose()?;
                for (when, then) in &case.branches {
                    let when = self.eval(when, row)?;
                    let hit = match &scrutinee {
                        Some(scrutinee) => equals(scrutinee, &when) == Some(true),
                        None => truth(&when)? == Some(true),
                    };
                    if hit {
                        return self.eval(then, row);
                    }
                }
                match &case.otherwise {
                    Some(otherwise) => self.eval(otherwise, row),
                    None => Ok(Datum::Null),
                }
            }
            Expr::ListComprehension(comprehension) => {
                let Some(items) = list_operand(self.eval(&comprehension.source, row)?)? else {
                    return Ok(Datum::Null);
                };
                let mut local = row.clone();
                let mut out = Vec::new();
                for item in items {
                    set(&mut local, comprehension.slot, item.clone());
                    if let Some(filter) = &comprehension.filter
                        && !self.predicate(filter, &local)?
                    {
                        continue;
                    }
                    out.push(match &comprehension.projection {
                        Some(projection) => self.eval(projection, &local)?,
                        None => item,
                    });
                }
                Ok(Datum::List(out))
            }
            Expr::Quantified(quantified) => {
                let Some(items) = list_operand(self.eval(&quantified.source, row)?)? else {
                    return Ok(Datum::Null);
                };
                let mut local = row.clone();
                let mut trues = 0usize;
                let mut falses = 0usize;
                let mut nulls = 0usize;
                for item in items {
                    set(&mut local, quantified.slot, item);
                    match truth(&self.eval(&quantified.predicate, &local)?)? {
                        Some(true) => trues += 1,
                        Some(false) => falses += 1,
                        None => nulls += 1,
                    }
                }
                let unknown = nulls > 0;
                Ok(from_truth(match quantified.quantifier {
                    Quantifier::All if falses > 0 => Some(false),
                    Quantifier::All => (!unknown).then_some(true),
                    Quantifier::Any if trues > 0 => Some(true),
                    Quantifier::Any => (!unknown).then_some(false),
                    Quantifier::None if trues > 0 => Some(false),
                    Quantifier::None => (!unknown).then_some(true),
                    Quantifier::Single if trues > 1 => Some(false),
                    Quantifier::Single if unknown => None,
                    Quantifier::Single => Some(trues == 1),
                }))
            }
            Expr::Exists(query) => {
                let mut found = false;
                self.match_query(query, row, &mut |_| {
                    found = true;
                    Ok(false)
                })?;
                Ok(Datum::Boolean(found))
            }
            Expr::PatternComprehension(query, projection) => {
                let mut out = Vec::new();
                self.match_query(query, row, &mut |matched| {
                    out.push(self.eval(projection, matched)?);
                    Ok(true)
                })?;
                Ok(Datum::List(out))
            }
        }
    }

    fn index(&self, base: Datum, index: Datum) -> Result<Datum, Error> {
        match (base, index) {
            (Datum::Null, _) | (_, Datum::Null) => Ok(Datum::Null),
            (Datum::List(items), Datum::Integer(index)) => Ok(list_position(items.len(), index)
                .and_then(|position| items.into_iter().nth(position))
                .unwrap_or(Datum::Null)),
            (Datum::List(_), other) => Err(Error::new(
                ErrorKind::ListElementAccessByNonInteger,
                format!(
                    "a list index must be an integer, not a {}",
                    other.type_name()
                ),
            )),
            (Datum::Map(mut entries), Datum::String(key)) => {
                Ok(entries.remove(&key).unwrap_or(Datum::Null))
            }
            (base @ (Datum::Node(_) | Datum::Relationship(_)), Datum::String(key)) => {
                self.property(&base, &key)
            }
            (Datum::Map(_) | Datum::Node(_) | Datum::Relationship(_), other) => Err(Error::new(
                ErrorKind::MapElementAccessByNonString,
                format!("a map key must be a string, not a {}", other.type_name()),
            )),
            (other, _) => Err(type_error(format!("cannot index a {}", other.type_name()))),
        }
    }
}

pub(crate) fn set(row: &mut Row, slot: Slot, value: Datum) {
    if let Some(entry) = row.get_mut(slot) {
        *entry = value;
    }
}

fn list_operand(value: Datum) -> Result<Option<Vec<Datum>>, Error> {
    match value {
        Datum::Null => Ok(None),
        Datum::List(items) => Ok(Some(items)),
        other => Err(type_error(format!(
            "expected a list, got a {}",
            other.type_name()
        ))),
    }
}

fn list_position(length: usize, index: i64) -> Option<usize> {
    let length = i64::try_from(length).ok()?;
    let position = if index < 0 { length + index } else { index };
    if (0..length).contains(&position) {
        usize::try_from(position).ok()
    } else {
        None
    }
}

fn slice(base: Datum, from: Option<Datum>, to: Option<Datum>) -> Result<Datum, Error> {
    let items = match base {
        Datum::Null => return Ok(Datum::Null),
        Datum::List(items) => items,
        other => return Err(type_error(format!("cannot slice a {}", other.type_name()))),
    };
    let length = i64::try_from(items.len()).unwrap_or(i64::MAX);
    let bound = |bound: Option<Datum>, default: i64| -> Result<Option<i64>, Error> {
        match bound {
            None => Ok(Some(default)),
            Some(Datum::Null) => Ok(None),
            Some(Datum::Integer(value)) => {
                let value = if value < 0 { length + value } else { value };
                Ok(Some(value.clamp(0, length)))
            }
            Some(other) => Err(type_error(format!(
                "a slice bound must be an integer, not a {}",
                other.type_name()
            ))),
        }
    };
    let (Some(from), Some(to)) = (bound(from, 0)?, bound(to, length)?) else {
        return Ok(Datum::Null);
    };
    if from >= to {
        return Ok(Datum::List(Vec::new()));
    }
    let from = usize::try_from(from).unwrap_or(0);
    let to = usize::try_from(to).unwrap_or(0);
    Ok(Datum::List(
        items.into_iter().skip(from).take(to - from).collect(),
    ))
}

fn and(a: Option<bool>, b: Option<bool>) -> Option<bool> {
    match (a, b) {
        (Some(false), _) | (_, Some(false)) => Some(false),
        (Some(true), Some(true)) => Some(true),
        _ => None,
    }
}

fn or(a: Option<bool>, b: Option<bool>) -> Option<bool> {
    match (a, b) {
        (Some(true), _) | (_, Some(true)) => Some(true),
        (Some(false), Some(false)) => Some(false),
        _ => None,
    }
}

fn comparison(op: ComparisonOp, left: &Datum, right: &Datum) -> Option<bool> {
    match op {
        ComparisonOp::Equal => equals(left, right),
        ComparisonOp::NotEqual => equals(left, right).map(|equal| !equal),
        _ => match compare(left, right)? {
            Comparison::Unordered => Some(false),
            Comparison::Ordered(ordering) => Some(match op {
                ComparisonOp::Less => ordering.is_lt(),
                ComparisonOp::LessEqual => ordering.is_le(),
                ComparisonOp::Greater => ordering.is_gt(),
                _ => ordering.is_ge(),
            }),
        },
    }
}

fn binary(op: BinaryOp, lhs: Datum, rhs: Datum) -> Result<Datum, Error> {
    match op {
        BinaryOp::And => Ok(from_truth(and(truth(&lhs)?, truth(&rhs)?))),
        BinaryOp::Or => Ok(from_truth(or(truth(&lhs)?, truth(&rhs)?))),
        BinaryOp::Xor => {
            let (lhs, rhs) = (truth(&lhs)?, truth(&rhs)?);
            Ok(from_truth(lhs.zip(rhs).map(|(a, b)| a != b)))
        }
        BinaryOp::In => membership(&lhs, rhs),
        BinaryOp::StartsWith | BinaryOp::EndsWith | BinaryOp::Contains => Ok(match (&lhs, &rhs) {
            (Datum::String(text), Datum::String(part)) => Datum::Boolean(match op {
                BinaryOp::StartsWith => text.starts_with(part.as_str()),
                BinaryOp::EndsWith => text.ends_with(part.as_str()),
                _ => text.contains(part.as_str()),
            }),
            _ => Datum::Null,
        }),
        BinaryOp::RegexMatch => Err(Error::new(
            ErrorKind::Unsupported,
            "regular expressions are not supported",
        )),
        BinaryOp::Add => add(lhs, rhs),
        BinaryOp::Subtract
        | BinaryOp::Multiply
        | BinaryOp::Divide
        | BinaryOp::Modulo
        | BinaryOp::Power => arithmetic(op, lhs, rhs),
    }
}

fn membership(needle: &Datum, haystack: Datum) -> Result<Datum, Error> {
    let items = match haystack {
        Datum::Null => return Ok(Datum::Null),
        Datum::List(items) => items,
        other => {
            return Err(type_error(format!(
                "IN needs a list on the right, not a {}",
                other.type_name()
            )));
        }
    };
    let mut unknown = false;
    for item in &items {
        match equals(needle, item) {
            Some(true) => return Ok(Datum::Boolean(true)),
            None => unknown = true,
            Some(false) => {}
        }
    }
    Ok(if unknown {
        Datum::Null
    } else {
        Datum::Boolean(false)
    })
}

fn add(lhs: Datum, rhs: Datum) -> Result<Datum, Error> {
    match (lhs, rhs) {
        (Datum::Null, _) | (_, Datum::Null) => Ok(Datum::Null),
        (Datum::List(mut a), Datum::List(b)) => {
            a.extend(b);
            Ok(Datum::List(a))
        }
        (Datum::List(mut a), b) => {
            a.push(b);
            Ok(Datum::List(a))
        }
        (a, Datum::List(mut b)) => {
            b.insert(0, a);
            Ok(Datum::List(b))
        }
        (Datum::String(a), Datum::String(b)) => Ok(Datum::String(a + &b)),
        (Datum::String(a), b @ (Datum::Integer(_) | Datum::Float(_))) => {
            Ok(Datum::String(a + &number_text(&b)))
        }
        (a @ (Datum::Integer(_) | Datum::Float(_)), Datum::String(b)) => {
            Ok(Datum::String(number_text(&a) + &b))
        }
        (lhs, rhs) => arithmetic(BinaryOp::Add, lhs, rhs),
    }
}

/// Formats a number the way `toString` does.
pub(crate) fn number_text(value: &Datum) -> String {
    match value {
        Datum::Integer(value) => value.to_string(),
        Datum::Float(value) => float_text(*value),
        _ => String::new(),
    }
}

pub(crate) fn float_text(value: f64) -> String {
    if value.is_nan() {
        "NaN".to_owned()
    } else if value.is_infinite() {
        if value > 0.0 { "Infinity" } else { "-Infinity" }.to_owned()
    } else if value == 0.0 || (1e-3..1e7).contains(&value.abs()) {
        with_fraction(format!("{value}"))
    } else {
        let text = format!("{value:e}");
        match text.split_once('e') {
            Some((mantissa, exponent)) => format!("{}E{exponent}", with_fraction(mantissa.into())),
            None => text,
        }
    }
}

fn with_fraction(text: String) -> String {
    if text.contains('.') {
        text
    } else {
        text + ".0"
    }
}

fn arithmetic(op: BinaryOp, lhs: Datum, rhs: Datum) -> Result<Datum, Error> {
    match (lhs, rhs) {
        (Datum::Null, _) | (_, Datum::Null) => Ok(Datum::Null),
        (Datum::Integer(a), Datum::Integer(b)) => integer_arithmetic(op, a, b),
        (Datum::Integer(a), Datum::Float(b)) => Ok(float_arithmetic(op, a as f64, b)),
        (Datum::Float(a), Datum::Integer(b)) => Ok(float_arithmetic(op, a, b as f64)),
        (Datum::Float(a), Datum::Float(b)) => Ok(float_arithmetic(op, a, b)),
        (a, b) => Err(type_error(format!(
            "cannot apply an arithmetic operator to a {} and a {}",
            a.type_name(),
            b.type_name()
        ))),
    }
}

fn integer_arithmetic(op: BinaryOp, a: i64, b: i64) -> Result<Datum, Error> {
    let divide_by_zero = || Error::new(ErrorKind::DivisionByZero, "integer division by zero");
    let result = match op {
        BinaryOp::Add => a.checked_add(b),
        BinaryOp::Subtract => a.checked_sub(b),
        BinaryOp::Multiply => a.checked_mul(b),
        BinaryOp::Divide => {
            if b == 0 {
                return Err(divide_by_zero());
            }
            a.checked_div(b)
        }
        BinaryOp::Modulo => {
            if b == 0 {
                return Err(divide_by_zero());
            }
            Some(a.wrapping_rem(b))
        }
        _ => return Ok(Datum::Float((a as f64).powf(b as f64))),
    };
    result.map(Datum::Integer).ok_or_else(overflow)
}

fn float_arithmetic(op: BinaryOp, a: f64, b: f64) -> Datum {
    Datum::Float(match op {
        BinaryOp::Add => a + b,
        BinaryOp::Subtract => a - b,
        BinaryOp::Multiply => a * b,
        BinaryOp::Divide => a / b,
        BinaryOp::Modulo => a % b,
        _ => a.powf(b),
    })
}
