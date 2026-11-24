use std::cmp::Ordering;

use super::datum::Datum;
use crate::gql::ast::SortDirection;

/// `a = b` under three-valued logic. `None` is null.
pub(crate) fn equals(a: &Datum, b: &Datum) -> Option<bool> {
    match (a, b) {
        (Datum::Null, _) | (_, Datum::Null) => None,
        (Datum::List(a), Datum::List(b)) => {
            if a.len() != b.len() {
                return Some(false);
            }
            all_equal(a.iter().zip(b))
        }
        (Datum::Map(a), Datum::Map(b)) => {
            if a.len() != b.len() || a.keys().ne(b.keys()) {
                return Some(false);
            }
            all_equal(a.values().zip(b.values()))
        }
        _ => match (number(a), number(b)) {
            (Some(a), Some(b)) => Some(a.compare(b) == Some(Ordering::Equal)),
            (None, None) => Some(scalar_equal(a, b)),
            _ => Some(false),
        },
    }
}

fn all_equal<'a>(pairs: impl Iterator<Item = (&'a Datum, &'a Datum)>) -> Option<bool> {
    let mut unknown = false;
    for (a, b) in pairs {
        match equals(a, b) {
            Some(false) => return Some(false),
            None => unknown = true,
            Some(true) => {}
        }
    }
    if unknown { None } else { Some(true) }
}

fn scalar_equal(a: &Datum, b: &Datum) -> bool {
    match (a, b) {
        (Datum::Boolean(a), Datum::Boolean(b)) => a == b,
        (Datum::String(a), Datum::String(b)) => a == b,
        (Datum::Bytes(a), Datum::Bytes(b)) => a == b,
        (Datum::Node(a), Datum::Node(b)) => a == b,
        (Datum::Relationship(a), Datum::Relationship(b)) => a == b,
        (Datum::Path(a), Datum::Path(b)) => a == b,
        (Datum::Temporal(a), Datum::Temporal(b)) => a.order(b) == Ordering::Equal,
        _ => false,
    }
}

/// How `a` compares to `b` for `<`, `<=`, `>`, and `>=`. `None` is null: the values are not
/// comparable. A NaN operand compares as unordered, which makes every inequality false.
pub(crate) fn compare(a: &Datum, b: &Datum) -> Option<Comparison> {
    match (a, b) {
        (Datum::Null, _) | (_, Datum::Null) => None,
        (Datum::String(a), Datum::String(b)) => Some(Comparison::Ordered(a.cmp(b))),
        (Datum::Boolean(a), Datum::Boolean(b)) => Some(Comparison::Ordered(a.cmp(b))),
        (Datum::Temporal(a), Datum::Temporal(b)) => a.compare(b).map(Comparison::Ordered),
        (Datum::List(a), Datum::List(b)) => {
            for (x, y) in a.iter().zip(b) {
                match compare(x, y)? {
                    Comparison::Ordered(Ordering::Equal) => {}
                    decided => return Some(decided),
                }
            }
            Some(Comparison::Ordered(a.len().cmp(&b.len())))
        }
        _ => {
            let (a, b) = (number(a)?, number(b)?);
            Some(match a.compare(b) {
                Some(ordering) => Comparison::Ordered(ordering),
                None => Comparison::Unordered,
            })
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Comparison {
    Ordered(Ordering),
    Unordered,
}

#[derive(Clone, Copy, Debug)]
enum Number {
    Integer(i64),
    Float(f64),
}

fn number(value: &Datum) -> Option<Number> {
    match value {
        Datum::Integer(value) => Some(Number::Integer(*value)),
        Datum::Float(value) => Some(Number::Float(*value)),
        _ => None,
    }
}

impl Number {
    fn compare(self, other: Number) -> Option<Ordering> {
        match (self, other) {
            (Number::Integer(a), Number::Integer(b)) => Some(a.cmp(&b)),
            (Number::Float(a), Number::Float(b)) => a.partial_cmp(&b),
            (Number::Integer(a), Number::Float(b)) => integer_float(a, b),
            (Number::Float(a), Number::Integer(b)) => integer_float(b, a).map(Ordering::reverse),
        }
    }

    fn total(self, other: Number) -> Ordering {
        let nan = |number: Number| matches!(number, Number::Float(value) if value.is_nan());
        match (nan(self), nan(other)) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            (false, false) => self.compare(other).unwrap_or(Ordering::Equal),
        }
    }
}

const TWO_TO_63: f64 = 9_223_372_036_854_775_808.0;

fn integer_float(integer: i64, float: f64) -> Option<Ordering> {
    if float.is_nan() {
        return None;
    }
    if float >= TWO_TO_63 {
        return Some(Ordering::Less);
    }
    if float < -TWO_TO_63 {
        return Some(Ordering::Greater);
    }
    let whole = float.trunc();
    let truncated = whole as i64;
    match integer.cmp(&truncated) {
        Ordering::Equal => 0.0.partial_cmp(&(float - whole)),
        ordering => Some(ordering),
    }
}

/// The global sort order used by `ORDER BY`: maps, nodes, relationships, lists, paths, temporal
/// values, strings, byte strings, booleans, numbers, then null. NaN sorts above every other number.
///
/// Two values are equivalent for `DISTINCT` and grouping exactly when this returns `Equal`.
/// How `a` sorts before or after `b` in `direction`, where nulls go first or last whichever way
/// the values sort.
pub(crate) fn sort_order(direction: SortDirection, a: &Datum, b: &Datum) -> Ordering {
    let (descending, nulls_first) = match direction {
        SortDirection::Ascending => (false, false),
        SortDirection::Descending => (true, true),
        SortDirection::AscendingNullsFirst => (false, true),
        SortDirection::DescendingNullsLast => (true, false),
    };
    match (a.is_null(), b.is_null()) {
        (true, true) => Ordering::Equal,
        (true, false) if nulls_first => Ordering::Less,
        (true, false) => Ordering::Greater,
        (false, true) if nulls_first => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) if descending => order(a, b).reverse(),
        (false, false) => order(a, b),
    }
}

pub(crate) fn order(a: &Datum, b: &Datum) -> Ordering {
    let rank = rank(a).cmp(&rank(b));
    if rank != Ordering::Equal {
        return rank;
    }
    match (a, b) {
        (Datum::Map(a), Datum::Map(b)) => {
            let keys = a.keys().cmp(b.keys());
            if keys != Ordering::Equal {
                return keys;
            }
            sequence(a.values(), b.values())
        }
        (Datum::Node(a), Datum::Node(b)) => a.cmp(b),
        (Datum::Relationship(a), Datum::Relationship(b)) => a.cmp(b),
        (Datum::List(a), Datum::List(b)) => sequence(a.iter(), b.iter()),
        (Datum::Path(a), Datum::Path(b)) => a
            .nodes
            .cmp(&b.nodes)
            .then_with(|| a.relationships.cmp(&b.relationships)),
        (Datum::Temporal(a), Datum::Temporal(b)) => a.order(b),
        (Datum::String(a), Datum::String(b)) => a.cmp(b),
        (Datum::Bytes(a), Datum::Bytes(b)) => a.cmp(b),
        (Datum::Boolean(a), Datum::Boolean(b)) => a.cmp(b),
        _ => match (number(a), number(b)) {
            (Some(a), Some(b)) => a.total(b),
            _ => Ordering::Equal,
        },
    }
}

fn sequence<'a>(
    a: impl Iterator<Item = &'a Datum>,
    b: impl Iterator<Item = &'a Datum>,
) -> Ordering {
    let mut b = b;
    for x in a {
        match b.next() {
            None => return Ordering::Greater,
            Some(y) => {
                let ordering = order(x, y);
                if ordering != Ordering::Equal {
                    return ordering;
                }
            }
        }
    }
    if b.next().is_some() {
        Ordering::Less
    } else {
        Ordering::Equal
    }
}

fn rank(value: &Datum) -> u8 {
    match value {
        Datum::Map(_) => 0,
        Datum::Node(_) => 1,
        Datum::Relationship(_) => 2,
        Datum::List(_) => 3,
        Datum::Path(_) => 4,
        Datum::Temporal(_) => 5,
        Datum::String(_) => 6,
        Datum::Bytes(_) => 7,
        Datum::Boolean(_) => 8,
        Datum::Integer(_) | Datum::Float(_) => 9,
        Datum::Null => 10,
    }
}

/// Values wrapped so that ordered collections group them by equivalence.
#[derive(Clone, Debug)]
pub(crate) struct Key(pub(crate) Vec<Datum>);

impl PartialEq for Key {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Key {}

impl PartialOrd for Key {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Key {
    fn cmp(&self, other: &Self) -> Ordering {
        sequence(self.0.iter(), other.0.iter())
    }
}

#[cfg(test)]
#[path = "compare.test.rs"]
mod tests;
