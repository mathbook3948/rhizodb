use crate::error::ErrorKind;
use crate::gql::ast::BinaryOp;
use crate::gql::ast::UnaryOp;

use super::datum::Datum;

/// A kind of value, as far as the types an operator or function accepts tell them apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Shape {
    Node,
    Relationship,
    Path,
    List,
    Map,
    String,
    Integer,
    Float,
    Boolean,
    Bytes,
}

/// Integers and floats.
pub(crate) const NUMBER: &[Shape] = &[Shape::Integer, Shape::Float];

/// Whether a position that takes the `accepted` shapes takes `shape`. No shapes means any.
pub(crate) fn admits(accepted: &[Shape], shape: Shape) -> bool {
    accepted.is_empty() || accepted.contains(&shape)
}

impl Shape {
    /// The shape of a value, or `None` for null, which every position takes.
    pub(crate) fn of(value: &Datum) -> Option<Shape> {
        match value {
            Datum::Null => None,
            Datum::Boolean(_) => Some(Shape::Boolean),
            Datum::Integer(_) => Some(Shape::Integer),
            Datum::Float(_) => Some(Shape::Float),
            Datum::String(_) => Some(Shape::String),
            Datum::Bytes(_) => Some(Shape::Bytes),
            Datum::List(_) => Some(Shape::List),
            Datum::Map(_) => Some(Shape::Map),
            Datum::Node(_) => Some(Shape::Node),
            Datum::Relationship(_) => Some(Shape::Relationship),
            Datum::Path(_) => Some(Shape::Path),
        }
    }
}

/// The error a unary operator raises on an operand of `operand`, or none when it takes it. Null
/// is taken by every operator, so the shapes here are never null.
pub(crate) fn unary_error(op: UnaryOp, operand: Shape) -> Option<ErrorKind> {
    let takes = match op {
        UnaryOp::Not => operand == Shape::Boolean,
        UnaryOp::Negate | UnaryOp::Plus => NUMBER.contains(&operand),
    };
    (!takes).then_some(ErrorKind::TypeMismatch)
}

/// The error a binary operator raises on operands of these shapes, or none when it takes them.
pub(crate) fn binary_error(op: BinaryOp, lhs: Shape, rhs: Shape) -> Option<ErrorKind> {
    let number = |shape: Shape| NUMBER.contains(&shape);
    let takes = match op {
        BinaryOp::And | BinaryOp::Or | BinaryOp::Xor => {
            lhs == Shape::Boolean && rhs == Shape::Boolean
        }
        BinaryOp::In => rhs == Shape::List,
        BinaryOp::StartsWith | BinaryOp::EndsWith | BinaryOp::Contains | BinaryOp::RegexMatch => {
            true
        }
        BinaryOp::Add => {
            lhs == Shape::List
                || rhs == Shape::List
                || ((lhs == Shape::String || number(lhs)) && (rhs == Shape::String || number(rhs)))
        }
        BinaryOp::Subtract
        | BinaryOp::Multiply
        | BinaryOp::Divide
        | BinaryOp::Modulo
        | BinaryOp::Power => number(lhs) && number(rhs),
    };
    (!takes).then_some(ErrorKind::TypeMismatch)
}

/// The error indexing a `base` with an `index` raises, or none when it takes them.
pub(crate) fn index_error(base: Shape, index: Shape) -> Option<ErrorKind> {
    match base {
        Shape::List => {
            (index != Shape::Integer).then_some(ErrorKind::ListElementAccessByNonInteger)
        }
        Shape::Map | Shape::Node | Shape::Relationship => {
            (index != Shape::String).then_some(ErrorKind::MapElementAccessByNonString)
        }
        _ => Some(ErrorKind::TypeMismatch),
    }
}

/// The error slicing a `base` raises, or none when it takes it.
pub(crate) fn slice_error(base: Shape) -> Option<ErrorKind> {
    (base != Shape::List).then_some(ErrorKind::TypeMismatch)
}

/// The error a slice bound of `bound` raises, or none when it takes it.
pub(crate) fn slice_bound_error(bound: Shape) -> Option<ErrorKind> {
    (bound != Shape::Integer).then_some(ErrorKind::TypeMismatch)
}

/// The error a label test raises on an operand of `operand`, or none when it takes it.
pub(crate) fn label_test_error(operand: Shape) -> Option<ErrorKind> {
    (operand != Shape::Node).then_some(ErrorKind::TypeMismatch)
}
