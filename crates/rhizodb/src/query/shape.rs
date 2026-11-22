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
