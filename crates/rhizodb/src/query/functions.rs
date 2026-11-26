use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::sync::Arc;

use unicode_normalization::UnicodeNormalization;

use crate::error::Error;
use crate::error::ErrorKind;
use crate::storage::ReadTransaction;

use super::compare::equals;
use super::datum;
use super::datum::Datum;
use super::eval::Env;
use super::eval::arithmetic;
use super::eval::float_text;
use super::scalars;
use super::shape::INSTANT;
use super::shape::NUMBER;
use super::shape::Shape;
use super::shape::admits;
use super::temporals::Moment;
use super::temporals::TemporalFunction;
use super::types;
use crate::gql::ast::BinaryOp;
use crate::gql::ast::NormalForm;
use crate::temporal::arithmetic::Span;
use crate::temporal::build::Kind;

/// A scalar function, resolved by name when the query is bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Function {
    Id,
    Labels,
    Type,
    Keys,
    Properties,
    Size,
    Length,
    Head,
    Last,
    Tail,
    Range,
    Reverse,
    Coalesce,
    ToInteger,
    ToFloat,
    ToString,
    ToBoolean,
    Abs,
    Sign,
    Ceil,
    Floor,
    Round,
    Sqrt,
    Exp,
    Log,
    Log10,
    Sin,
    Cos,
    Tan,
    Asin,
    Acos,
    Atan,
    Atan2,
    Degrees,
    Radians,
    Pi,
    E,
    Rand,
    StartNode,
    EndNode,
    Nodes,
    Relationships,
    ToUpper,
    ToLower,
    Trim,
    LTrim,
    RTrim,
    Substring,
    Replace,
    Split,
    Left,
    Right,
    Exists,
    CharLength,
    ByteLength,
    Cardinality,
    IsEmpty,
    IsNaN,
    NullIf,
    ElementId,
    Power,
    Mod,
    Sinh,
    Cosh,
    Tanh,
    Cot,
    BTrim,
    ToStringOrNull,
    ToIntegerOrNull,
    ToFloatOrNull,
    ToBooleanOrNull,
    Normalize,
    ValueType,
    RandomUuid,
    Haversin,
    Timestamp,
    ToBooleanList,
    ToIntegerList,
    ToFloatList,
    ToStringList,
    CosineSimilarity,
    EuclideanSimilarity,
    Temporal(TemporalFunction),
}

const MAX_RANGE_LENGTH: i128 = 10_000_000;

type Accepted = &'static [Shape];

const ANY: Accepted = &[];
const INTEGER: Accepted = &[Shape::Integer];
const STRING: Accepted = &[Shape::String];
const LIST: Accepted = &[Shape::List];
const PATH: Accepted = &[Shape::Path];
const NODE: Accepted = &[Shape::Node];
const RELATIONSHIP: Accepted = &[Shape::Relationship];
const ELEMENT: Accepted = &[Shape::Node, Shape::Relationship];
const ENTITY: Accepted = &[Shape::Node, Shape::Relationship, Shape::Map];
const LIST_OR_STRING: Accepted = &[Shape::List, Shape::String];
const LIST_STRING_OR_MAP: Accepted = &[Shape::List, Shape::String, Shape::Map];
const INTEGER_CONVERTIBLE: Accepted =
    &[Shape::Integer, Shape::Float, Shape::Boolean, Shape::String];
const FLOAT_CONVERTIBLE: Accepted = &[Shape::Integer, Shape::Float, Shape::String];
const BOOLEAN_CONVERTIBLE: Accepted = &[Shape::Boolean, Shape::Integer, Shape::String];
const STRING_CONVERTIBLE: Accepted = &[
    Shape::String,
    Shape::Integer,
    Shape::Float,
    Shape::Boolean,
    Shape::Date,
    Shape::LocalTime,
    Shape::Time,
    Shape::LocalDateTime,
    Shape::DateTime,
    Shape::Duration,
];
const MAP: Accepted = &[Shape::Map];
const DATE_SOURCE: Accepted = &[
    Shape::String,
    Shape::Map,
    Shape::Date,
    Shape::LocalDateTime,
    Shape::DateTime,
];
const TIME_SOURCE: Accepted = &[
    Shape::String,
    Shape::Map,
    Shape::LocalTime,
    Shape::Time,
    Shape::LocalDateTime,
    Shape::DateTime,
];
const DATETIME_SOURCE: Accepted = &[
    Shape::String,
    Shape::Map,
    Shape::LocalDateTime,
    Shape::DateTime,
];
const DURATION_SOURCE: Accepted = &[Shape::String, Shape::Map];
const DATED: Accepted = &[Shape::Date, Shape::LocalDateTime, Shape::DateTime];
const TIMED: Accepted = &[
    Shape::LocalTime,
    Shape::Time,
    Shape::LocalDateTime,
    Shape::DateTime,
];

const fn construct(kind: Kind) -> Function {
    Function::Temporal(TemporalFunction::Construct(kind))
}

const fn clock(kind: Kind, moment: Moment) -> Function {
    Function::Temporal(TemporalFunction::Clock(kind, moment))
}

const fn truncate(kind: Kind) -> Function {
    Function::Temporal(TemporalFunction::Truncate(kind))
}

const fn between(span: Span) -> Function {
    Function::Temporal(TemporalFunction::Between(span))
}

const SIGNATURES: [(&str, Function, usize, usize, &[Accepted]); 121] = [
    ("id", Function::Id, 1, 1, &[ELEMENT]),
    ("labels", Function::Labels, 1, 1, &[NODE]),
    ("type", Function::Type, 1, 1, &[RELATIONSHIP]),
    ("keys", Function::Keys, 1, 1, &[ENTITY]),
    ("properties", Function::Properties, 1, 1, &[ENTITY]),
    ("size", Function::Size, 1, 1, &[LIST_OR_STRING]),
    ("length", Function::Length, 1, 1, &[PATH]),
    ("head", Function::Head, 1, 1, &[LIST]),
    ("last", Function::Last, 1, 1, &[LIST]),
    ("tail", Function::Tail, 1, 1, &[LIST]),
    ("range", Function::Range, 2, 3, &[INTEGER]),
    ("reverse", Function::Reverse, 1, 1, &[LIST_OR_STRING]),
    ("coalesce", Function::Coalesce, 1, usize::MAX, &[ANY]),
    (
        "tointeger",
        Function::ToInteger,
        1,
        1,
        &[INTEGER_CONVERTIBLE],
    ),
    ("tofloat", Function::ToFloat, 1, 1, &[FLOAT_CONVERTIBLE]),
    ("tostring", Function::ToString, 1, 1, &[STRING_CONVERTIBLE]),
    (
        "toboolean",
        Function::ToBoolean,
        1,
        1,
        &[BOOLEAN_CONVERTIBLE],
    ),
    ("abs", Function::Abs, 1, 1, &[NUMBER]),
    ("sign", Function::Sign, 1, 1, &[NUMBER]),
    ("ceil", Function::Ceil, 1, 1, &[NUMBER]),
    ("ceiling", Function::Ceil, 1, 1, &[NUMBER]),
    ("floor", Function::Floor, 1, 1, &[NUMBER]),
    ("round", Function::Round, 1, 3, &[NUMBER, INTEGER, STRING]),
    ("sqrt", Function::Sqrt, 1, 1, &[NUMBER]),
    ("exp", Function::Exp, 1, 1, &[NUMBER]),
    ("log", Function::Log, 1, 2, &[NUMBER]),
    ("ln", Function::Log, 1, 2, &[NUMBER]),
    ("log10", Function::Log10, 1, 1, &[NUMBER]),
    ("sin", Function::Sin, 1, 1, &[NUMBER]),
    ("cos", Function::Cos, 1, 1, &[NUMBER]),
    ("tan", Function::Tan, 1, 1, &[NUMBER]),
    ("asin", Function::Asin, 1, 1, &[NUMBER]),
    ("acos", Function::Acos, 1, 1, &[NUMBER]),
    ("atan", Function::Atan, 1, 1, &[NUMBER]),
    ("atan2", Function::Atan2, 2, 2, &[NUMBER]),
    ("degrees", Function::Degrees, 1, 1, &[NUMBER]),
    ("radians", Function::Radians, 1, 1, &[NUMBER]),
    ("pi", Function::Pi, 0, 0, &[]),
    ("e", Function::E, 0, 0, &[]),
    ("rand", Function::Rand, 0, 0, &[]),
    ("startnode", Function::StartNode, 1, 1, &[RELATIONSHIP]),
    ("endnode", Function::EndNode, 1, 1, &[RELATIONSHIP]),
    ("nodes", Function::Nodes, 1, 1, &[PATH]),
    ("relationships", Function::Relationships, 1, 1, &[PATH]),
    ("toupper", Function::ToUpper, 1, 1, &[STRING]),
    ("upper", Function::ToUpper, 1, 1, &[STRING]),
    ("tolower", Function::ToLower, 1, 1, &[STRING]),
    ("lower", Function::ToLower, 1, 1, &[STRING]),
    ("trim", Function::Trim, 1, 2, &[STRING]),
    ("btrim", Function::BTrim, 1, 2, &[STRING]),
    ("ltrim", Function::LTrim, 1, 2, &[STRING]),
    ("rtrim", Function::RTrim, 1, 2, &[STRING]),
    ("char_length", Function::CharLength, 1, 1, &[STRING]),
    ("character_length", Function::CharLength, 1, 1, &[STRING]),
    ("byte_length", Function::ByteLength, 1, 1, &[STRING]),
    ("octet_length", Function::ByteLength, 1, 1, &[STRING]),
    ("cardinality", Function::Cardinality, 1, 1, &[LIST]),
    ("path_length", Function::Length, 1, 1, &[PATH]),
    ("isempty", Function::IsEmpty, 1, 1, &[LIST_STRING_OR_MAP]),
    ("isnan", Function::IsNaN, 1, 1, &[NUMBER]),
    ("nullif", Function::NullIf, 2, 2, &[ANY]),
    ("elementid", Function::ElementId, 1, 1, &[ELEMENT]),
    ("power", Function::Power, 2, 2, &[NUMBER]),
    ("mod", Function::Mod, 2, 2, &[NUMBER]),
    ("sinh", Function::Sinh, 1, 1, &[NUMBER]),
    ("cosh", Function::Cosh, 1, 1, &[NUMBER]),
    ("tanh", Function::Tanh, 1, 1, &[NUMBER]),
    ("cot", Function::Cot, 1, 1, &[NUMBER]),
    ("tostringornull", Function::ToStringOrNull, 1, 1, &[ANY]),
    ("tointegerornull", Function::ToIntegerOrNull, 1, 1, &[ANY]),
    ("tofloatornull", Function::ToFloatOrNull, 1, 1, &[ANY]),
    ("tobooleanornull", Function::ToBooleanOrNull, 1, 1, &[ANY]),
    ("substring", Function::Substring, 2, 3, &[STRING, INTEGER]),
    ("replace", Function::Replace, 3, 3, &[STRING]),
    ("split", Function::Split, 2, 2, &[STRING]),
    ("left", Function::Left, 2, 2, &[STRING, INTEGER]),
    ("right", Function::Right, 2, 2, &[STRING, INTEGER]),
    ("exists", Function::Exists, 1, 1, &[ANY]),
    ("normalize", Function::Normalize, 1, 2, &[STRING]),
    ("valuetype", Function::ValueType, 1, 1, &[ANY]),
    ("randomuuid", Function::RandomUuid, 0, 0, &[]),
    ("haversin", Function::Haversin, 1, 1, &[NUMBER]),
    ("timestamp", Function::Timestamp, 0, 0, &[]),
    ("tobooleanlist", Function::ToBooleanList, 1, 1, &[LIST]),
    ("tointegerlist", Function::ToIntegerList, 1, 1, &[LIST]),
    ("tofloatlist", Function::ToFloatList, 1, 1, &[LIST]),
    ("tostringlist", Function::ToStringList, 1, 1, &[LIST]),
    (
        "vector.similarity.cosine",
        Function::CosineSimilarity,
        2,
        2,
        &[LIST],
    ),
    (
        "vector.similarity.euclidean",
        Function::EuclideanSimilarity,
        2,
        2,
        &[LIST],
    ),
    ("date", construct(Kind::Date), 0, 1, &[DATE_SOURCE]),
    (
        "localtime",
        construct(Kind::LocalTime),
        0,
        1,
        &[TIME_SOURCE],
    ),
    ("time", construct(Kind::Time), 0, 1, &[TIME_SOURCE]),
    (
        "localdatetime",
        construct(Kind::LocalDateTime),
        0,
        1,
        &[DATETIME_SOURCE],
    ),
    (
        "datetime",
        construct(Kind::DateTime),
        0,
        1,
        &[DATETIME_SOURCE],
    ),
    (
        "date.transaction",
        clock(Kind::Date, Moment::Transaction),
        0,
        1,
        &[STRING],
    ),
    (
        "date.statement",
        clock(Kind::Date, Moment::Statement),
        0,
        1,
        &[STRING],
    ),
    (
        "date.realtime",
        clock(Kind::Date, Moment::Realtime),
        0,
        1,
        &[STRING],
    ),
    (
        "localtime.transaction",
        clock(Kind::LocalTime, Moment::Transaction),
        0,
        1,
        &[STRING],
    ),
    (
        "localtime.statement",
        clock(Kind::LocalTime, Moment::Statement),
        0,
        1,
        &[STRING],
    ),
    (
        "localtime.realtime",
        clock(Kind::LocalTime, Moment::Realtime),
        0,
        1,
        &[STRING],
    ),
    (
        "time.transaction",
        clock(Kind::Time, Moment::Transaction),
        0,
        1,
        &[STRING],
    ),
    (
        "time.statement",
        clock(Kind::Time, Moment::Statement),
        0,
        1,
        &[STRING],
    ),
    (
        "time.realtime",
        clock(Kind::Time, Moment::Realtime),
        0,
        1,
        &[STRING],
    ),
    (
        "localdatetime.transaction",
        clock(Kind::LocalDateTime, Moment::Transaction),
        0,
        1,
        &[STRING],
    ),
    (
        "localdatetime.statement",
        clock(Kind::LocalDateTime, Moment::Statement),
        0,
        1,
        &[STRING],
    ),
    (
        "localdatetime.realtime",
        clock(Kind::LocalDateTime, Moment::Realtime),
        0,
        1,
        &[STRING],
    ),
    (
        "datetime.transaction",
        clock(Kind::DateTime, Moment::Transaction),
        0,
        1,
        &[STRING],
    ),
    (
        "datetime.statement",
        clock(Kind::DateTime, Moment::Statement),
        0,
        1,
        &[STRING],
    ),
    (
        "datetime.realtime",
        clock(Kind::DateTime, Moment::Realtime),
        0,
        1,
        &[STRING],
    ),
    (
        "date.truncate",
        truncate(Kind::Date),
        2,
        3,
        &[STRING, DATED, MAP],
    ),
    (
        "localtime.truncate",
        truncate(Kind::LocalTime),
        2,
        3,
        &[STRING, TIMED, MAP],
    ),
    (
        "time.truncate",
        truncate(Kind::Time),
        2,
        3,
        &[STRING, TIMED, MAP],
    ),
    (
        "localdatetime.truncate",
        truncate(Kind::LocalDateTime),
        2,
        3,
        &[STRING, DATED, MAP],
    ),
    (
        "datetime.truncate",
        truncate(Kind::DateTime),
        2,
        3,
        &[STRING, DATED, MAP],
    ),
    (
        "datetime.fromepoch",
        Function::Temporal(TemporalFunction::FromEpoch),
        2,
        2,
        &[INTEGER],
    ),
    (
        "datetime.fromepochmillis",
        Function::Temporal(TemporalFunction::FromEpochMillis),
        1,
        1,
        &[INTEGER],
    ),
    (
        "duration",
        Function::Temporal(TemporalFunction::Duration),
        1,
        1,
        &[DURATION_SOURCE],
    ),
    ("duration.between", between(Span::Whole), 2, 2, &[INSTANT]),
    ("duration.inmonths", between(Span::Months), 2, 2, &[INSTANT]),
    ("duration.indays", between(Span::Days), 2, 2, &[INSTANT]),
    (
        "duration.inseconds",
        between(Span::Seconds),
        2,
        2,
        &[INSTANT],
    ),
];

impl Function {
    pub(crate) fn lookup(name: &str) -> Option<Function> {
        SIGNATURES
            .iter()
            .find(|(candidate, ..)| candidate.eq_ignore_ascii_case(name))
            .map(|(_, function, ..)| *function)
    }

    pub(crate) fn accepts(self, count: usize) -> bool {
        SIGNATURES
            .iter()
            .find(|(_, function, ..)| *function == self)
            .is_some_and(|(_, _, min, max, _)| (*min..=*max).contains(&count))
    }

    /// The shapes the argument at `index` accepts, where none means any. The binder checks the
    /// arguments whose shape the query text decides against them, and a call checks the values it
    /// gets against them, so a function takes the same types whichever way they arrive.
    pub(crate) fn parameter(self, index: usize) -> &'static [Shape] {
        SIGNATURES
            .iter()
            .find(|(_, function, ..)| *function == self)
            .and_then(|(.., parameters)| parameters.get(index).or(parameters.last()))
            .copied()
            .unwrap_or(ANY)
    }

    /// The error kind for an argument of a type the function does not take, when it differs
    /// from the kind a type error has elsewhere: every argument of `range()` raises
    /// [`ErrorKind::NonIntegerArgument`].
    pub(crate) fn argument_error(self) -> Option<ErrorKind> {
        (self == Function::Range).then_some(ErrorKind::NonIntegerArgument)
    }

    fn name(self) -> &'static str {
        SIGNATURES
            .iter()
            .find(|(_, function, ..)| *function == self)
            .map_or("function", |(name, ..)| name)
    }
}

/// A function got a value whose type it cannot work with.
pub(crate) fn invalid_value(function: Function, value: &Datum) -> Error {
    Error::rejecting_type(
        ErrorKind::InvalidArgumentValue,
        format!("{}() cannot take a {}", function.name(), value.type_name()),
    )
}

fn integer(value: usize) -> Datum {
    Datum::Integer(i64::try_from(value).unwrap_or(i64::MAX))
}

fn random_unit() -> f64 {
    let bits = RandomState::new().hash_one(std::time::SystemTime::now());
    (bits >> 11) as f64 / (1u64 << 53) as f64
}

impl<T: ReadTransaction> Env<'_, T> {
    pub(crate) fn call(&self, function: Function, args: Vec<Datum>) -> Result<Datum, Error> {
        if function == Function::Coalesce {
            return Ok(args
                .into_iter()
                .find(|value| !value.is_null())
                .unwrap_or(Datum::Null));
        }
        for (index, value) in args.iter().enumerate() {
            if let Some(shape) = Shape::of(value)
                && !admits(function.parameter(index), shape)
            {
                let error = invalid_value(function, value);
                return Err(match function.argument_error() {
                    Some(kind) => error.with_kind(kind),
                    None => error,
                });
            }
        }
        let count = args.len();
        let mut args = args.into_iter();
        let first = args.next().unwrap_or(Datum::Null);
        let second = args.next();
        let third = args.next();
        if !matches!(
            function,
            Function::Pi
                | Function::E
                | Function::Rand
                | Function::Exists
                | Function::ValueType
                | Function::RandomUuid
                | Function::Timestamp
                | Function::Temporal(_)
        ) && first.is_null()
        {
            return Ok(Datum::Null);
        }
        let invalid = |value: &Datum| invalid_value(function, value);
        match function {
            Function::Coalesce => Ok(first),
            Function::NullIf => Ok(match second {
                Some(other) if equals(&first, &other) == Some(true) => Datum::Null,
                _ => first,
            }),
            Function::ElementId => match first {
                Datum::Node(id) => Ok(Datum::String(format!("node:{}", id.0))),
                Datum::Relationship(id) => Ok(Datum::String(format!("relationship:{}", id.0))),
                other => Err(invalid(&other)),
            },
            Function::CharLength => Ok(integer(string_argument(function, first)?.chars().count())),
            Function::ByteLength => Ok(integer(string_argument(function, first)?.len())),
            Function::Cardinality => match first {
                Datum::List(items) => Ok(integer(items.len())),
                other => Err(invalid(&other)),
            },
            Function::IsEmpty => match first {
                Datum::List(items) => Ok(Datum::Boolean(items.is_empty())),
                Datum::String(text) => Ok(Datum::Boolean(text.is_empty())),
                Datum::Map(entries) => Ok(Datum::Boolean(entries.is_empty())),
                other => Err(invalid(&other)),
            },
            Function::IsNaN => match first {
                Datum::Integer(_) => Ok(Datum::Boolean(false)),
                Datum::Float(value) => Ok(Datum::Boolean(value.is_nan())),
                other => Err(invalid(&other)),
            },
            Function::Power | Function::Mod => {
                let Some(second) = second.filter(|value| !value.is_null()) else {
                    return Ok(Datum::Null);
                };
                let op = if function == Function::Power {
                    BinaryOp::Power
                } else {
                    BinaryOp::Modulo
                };
                for value in [&first, &second] {
                    if !matches!(value, Datum::Integer(_) | Datum::Float(_)) {
                        return Err(invalid(value));
                    }
                }
                arithmetic(op, first, second)
            }
            Function::ToStringOrNull => Ok(to_string(first).unwrap_or(Datum::Null)),
            Function::ToIntegerOrNull => Ok(to_integer(first).unwrap_or(Datum::Null)),
            Function::ToFloatOrNull => Ok(to_float(first).unwrap_or(Datum::Null)),
            Function::ToBooleanOrNull => Ok(to_boolean(first).unwrap_or(Datum::Null)),
            Function::Exists => Ok(Datum::Boolean(!first.is_null())),
            Function::ValueType => Ok(Datum::String(types::type_name(&first))),
            Function::RandomUuid => Ok(scalars::random_uuid()),
            Function::Timestamp => Ok(Datum::Integer(
                i64::try_from(self.now.statement.epoch_nanos().div_euclid(1_000_000))
                    .unwrap_or(i64::MAX),
            )),
            Function::Haversin => Ok(Datum::Float(
                (1.0 - float_argument(function, &first)?.cos()) / 2.0,
            )),
            Function::ToBooleanList => scalars::convert_list("toBooleanList", first, |item| {
                to_boolean(item).ok().filter(|value| !value.is_null())
            }),
            Function::ToIntegerList => scalars::convert_list("toIntegerList", first, |item| {
                to_integer(item).ok().filter(|value| !value.is_null())
            }),
            Function::ToFloatList => scalars::convert_list("toFloatList", first, |item| {
                to_float(item).ok().filter(|value| !value.is_null())
            }),
            Function::ToStringList => scalars::convert_list("toStringList", first, |item| {
                to_string(item).ok().filter(|value| !value.is_null())
            }),
            Function::CosineSimilarity | Function::EuclideanSimilarity => {
                let name = function.name();
                let Some(second) = second.filter(|value| !value.is_null()) else {
                    return Ok(Datum::Null);
                };
                match (
                    scalars::vector(name, &first)?,
                    scalars::vector(name, &second)?,
                ) {
                    (Some(a), Some(b)) => {
                        scalars::similarity(name, function == Function::CosineSimilarity, &a, &b)
                    }
                    _ => Ok(Datum::Null),
                }
            }
            Function::Round if count > 1 => {
                let value = float_argument(function, &first)?;
                let precision = match second {
                    Some(Datum::Integer(precision)) => precision,
                    Some(Datum::Null) | None => return Ok(Datum::Null),
                    Some(other) => return Err(invalid(&other)),
                };
                match third {
                    None => scalars::round(value, precision, None),
                    Some(Datum::Null) => Ok(Datum::Null),
                    Some(Datum::String(mode)) => scalars::round(value, precision, Some(&mode)),
                    Some(other) => Err(invalid(&other)),
                }
            }
            Function::Normalize => {
                let text = string_argument(function, first)?;
                let form = match second {
                    None => NormalForm::Nfc,
                    Some(Datum::Null) => return Ok(Datum::Null),
                    Some(Datum::String(name)) => NormalForm::named(&name).ok_or_else(|| {
                        Error::new(
                            ErrorKind::InvalidArgumentValue,
                            format!("normalize() knows no normal form `{name}`"),
                        )
                    })?,
                    Some(other) => return Err(invalid(&other)),
                };
                Ok(Datum::String(normalized(&text, form)))
            }
            Function::Pi => Ok(Datum::Float(std::f64::consts::PI)),
            Function::E => Ok(Datum::Float(std::f64::consts::E)),
            Function::Rand => Ok(Datum::Float(random_unit())),
            Function::Id => match first {
                Datum::Node(id) => Ok(Datum::Integer(id.0 as i64)),
                Datum::Relationship(id) => Ok(Datum::Integer(id.0 as i64)),
                other => Err(invalid(&other)),
            },
            Function::Labels => match first {
                Datum::Node(id) => Ok(Datum::List(Arc::new(
                    self.node_label_names(id)?
                        .into_iter()
                        .map(Datum::String)
                        .collect(),
                ))),
                other => Err(invalid(&other)),
            },
            Function::Type => match first {
                Datum::Relationship(id) => {
                    Ok(Datum::String(self.relationship_type(id)?.to_string()))
                }
                other => Err(invalid(&other)),
            },
            Function::Keys => {
                let names = match first {
                    Datum::Node(id) => self.node_keys(id)?,
                    Datum::Relationship(id) => self.relationship_keys(id)?,
                    Datum::Map(entries) => entries.keys().cloned().collect(),
                    other => return Err(invalid(&other)),
                };
                Ok(Datum::List(Arc::new(
                    names
                        .iter()
                        .map(|name| Datum::String(name.to_string()))
                        .collect(),
                )))
            }
            Function::Properties => Ok(Datum::Map(match first {
                Datum::Node(id) => Arc::new(self.property_map(&self.node(id)?.properties)?),
                Datum::Relationship(id) => {
                    Arc::new(self.property_map(&self.relationship(id)?.properties)?)
                }
                Datum::Map(entries) => entries,
                other => return Err(invalid(&other)),
            })),
            Function::Size => match first {
                Datum::List(items) => Ok(integer(items.len())),
                Datum::String(text) => Ok(integer(text.chars().count())),
                other => Err(invalid(&other)),
            },
            Function::Length => match first {
                Datum::Path(path) => Ok(integer(path.relationships.len())),
                other => Err(invalid(&other)),
            },
            Function::Head | Function::Last | Function::Tail => match first {
                Datum::List(items) => Ok(match function {
                    Function::Head => items.first().cloned().unwrap_or(Datum::Null),
                    Function::Last => items.last().cloned().unwrap_or(Datum::Null),
                    _ => Datum::List(Arc::new(items.iter().skip(1).cloned().collect())),
                }),
                other => Err(invalid(&other)),
            },
            Function::Reverse => match first {
                Datum::List(mut items) => {
                    Arc::make_mut(&mut items).reverse();
                    Ok(Datum::List(items))
                }
                Datum::String(text) => Ok(Datum::String(text.chars().rev().collect())),
                other => Err(invalid(&other)),
            },
            Function::Range => range(first, second, third),
            Function::ToInteger => to_integer(first),
            Function::ToFloat => to_float(first),
            Function::ToString => to_string(first),
            Function::ToBoolean => to_boolean(first),
            Function::Abs => match first {
                Datum::Integer(value) => value
                    .checked_abs()
                    .map(Datum::Integer)
                    .ok_or_else(|| Error::new(ErrorKind::IntegerOverflow, "abs() overflowed")),
                Datum::Float(value) => Ok(Datum::Float(value.abs())),
                other => Err(invalid(&other)),
            },
            Function::Sign => match first {
                Datum::Integer(value) => Ok(Datum::Integer(value.signum())),
                Datum::Float(value) if value == 0.0 || value.is_nan() => Ok(Datum::Integer(0)),
                Datum::Float(value) => Ok(Datum::Integer(if value > 0.0 { 1 } else { -1 })),
                other => Err(invalid(&other)),
            },
            Function::Atan2 => {
                let y = float_argument(function, &first)?;
                match second {
                    Some(Datum::Null) | None => Ok(Datum::Null),
                    Some(x) => Ok(Datum::Float(y.atan2(float_argument(function, &x)?))),
                }
            }
            Function::Ceil
            | Function::Floor
            | Function::Round
            | Function::Sqrt
            | Function::Exp
            | Function::Log
            | Function::Log10
            | Function::Sinh
            | Function::Cosh
            | Function::Tanh
            | Function::Cot
            | Function::Sin
            | Function::Cos
            | Function::Tan
            | Function::Asin
            | Function::Acos
            | Function::Atan
            | Function::Degrees
            | Function::Radians => {
                let value = float_argument(function, &first)?;
                if function == Function::Log && count == 2 {
                    return match second {
                        Some(Datum::Null) | None => Ok(Datum::Null),
                        Some(argument) => Ok(Datum::Float(
                            float_argument(function, &argument)?.log(value),
                        )),
                    };
                }
                Ok(Datum::Float(match function {
                    Function::Ceil => value.ceil(),
                    Function::Floor => value.floor(),
                    Function::Round => round_half_up(value),
                    Function::Sqrt => value.sqrt(),
                    Function::Exp => value.exp(),
                    Function::Log => value.ln(),
                    Function::Log10 => value.log10(),
                    Function::Sinh => value.sinh(),
                    Function::Cosh => value.cosh(),
                    Function::Tanh => value.tanh(),
                    Function::Cot => 1.0 / value.tan(),
                    Function::Sin => value.sin(),
                    Function::Cos => value.cos(),
                    Function::Tan => value.tan(),
                    Function::Asin => value.asin(),
                    Function::Acos => value.acos(),
                    Function::Atan => value.atan(),
                    Function::Degrees => value.to_degrees(),
                    _ => value.to_radians(),
                }))
            }
            Function::StartNode | Function::EndNode => match first {
                Datum::Relationship(id) => {
                    let record = self.relationship(id)?;
                    Ok(Datum::Node(if function == Function::StartNode {
                        record.source
                    } else {
                        record.target
                    }))
                }
                other => Err(invalid(&other)),
            },
            Function::Nodes => match first {
                Datum::Path(path) => Ok(Datum::List(Arc::new(
                    path.nodes.into_iter().map(Datum::Node).collect(),
                ))),
                other => Err(invalid(&other)),
            },
            Function::Relationships => match first {
                Datum::Path(path) => Ok(Datum::List(Arc::new(
                    path.relationships
                        .into_iter()
                        .map(Datum::Relationship)
                        .collect(),
                ))),
                other => Err(invalid(&other)),
            },
            Function::ToUpper | Function::ToLower => match first {
                Datum::String(text) => Ok(Datum::String(if function == Function::ToUpper {
                    text.to_uppercase()
                } else {
                    text.to_lowercase()
                })),
                other => Err(invalid(&other)),
            },
            Function::Trim | Function::BTrim | Function::LTrim | Function::RTrim => {
                let text = string_argument(function, first)?;
                let characters = match second {
                    None => None,
                    Some(Datum::Null) => return Ok(Datum::Null),
                    Some(Datum::String(characters)) => Some(characters),
                    Some(other) => return Err(invalid(&other)),
                };
                let trimmed = |text: &str| -> String {
                    let start = function != Function::RTrim;
                    let end = function != Function::LTrim;
                    let strip = |c: char| match &characters {
                        Some(characters) => characters.contains(c),
                        None => c.is_whitespace(),
                    };
                    let text = if start {
                        text.trim_start_matches(strip)
                    } else {
                        text
                    };
                    let text = if end {
                        text.trim_end_matches(strip)
                    } else {
                        text
                    };
                    text.to_owned()
                };
                Ok(Datum::String(trimmed(&text)))
            }
            Function::Substring => {
                let text = string_argument(function, first)?;
                let Some(start) = integer_argument(function, second)? else {
                    return Ok(Datum::Null);
                };
                let length = match third {
                    None => None,
                    Some(length) => match integer_argument(function, Some(length))? {
                        Some(length) => Some(length),
                        None => return Ok(Datum::Null),
                    },
                };
                let start = non_negative(start)?;
                let chars = text.chars().skip(start);
                Ok(Datum::String(match length {
                    Some(length) => chars.take(non_negative(length)?).collect(),
                    None => chars.collect(),
                }))
            }
            Function::Left | Function::Right => {
                let text = string_argument(function, first)?;
                let Some(count) = integer_argument(function, second)? else {
                    return Ok(Datum::Null);
                };
                let count = non_negative(count)?;
                let chars: Vec<char> = text.chars().collect();
                let count = count.min(chars.len());
                Ok(Datum::String(if function == Function::Left {
                    chars[..count].iter().collect()
                } else {
                    chars[chars.len() - count..].iter().collect()
                }))
            }
            Function::Replace => {
                let text = string_argument(function, first)?;
                let (Some(Datum::String(from)), Some(Datum::String(to))) = (&second, &third) else {
                    return match (second, third) {
                        (Some(Datum::Null), _) | (_, Some(Datum::Null)) | (None, _) => {
                            Ok(Datum::Null)
                        }
                        (Some(Datum::String(_)), Some(other)) | (Some(other), _) => {
                            Err(invalid(&other))
                        }
                    };
                };
                if to.len() > from.len() {
                    let matches = if from.is_empty() {
                        text.chars().count().saturating_add(1)
                    } else {
                        text.matches(from.as_str()).count()
                    };
                    datum::check_string_bytes(
                        text.len()
                            .saturating_add(matches.saturating_mul(to.len() - from.len())),
                    )?;
                }
                Ok(Datum::String(text.replace(from.as_str(), to)))
            }
            Function::Temporal(temporal) => {
                let args = std::iter::once(first).chain(second).chain(third);
                self.call_temporal(temporal, args.take(count).collect())
            }
            Function::Split => {
                let text = string_argument(function, first)?;
                match second {
                    Some(Datum::String(delimiter)) if delimiter.is_empty() => {
                        Ok(Datum::List(Arc::new(
                            text.chars()
                                .map(|char| Datum::String(char.to_string()))
                                .collect(),
                        )))
                    }
                    Some(Datum::String(delimiter)) => Ok(Datum::List(Arc::new(
                        text.split(delimiter.as_str())
                            .map(|part| Datum::String(part.to_owned()))
                            .collect(),
                    ))),
                    Some(Datum::Null) | None => Ok(Datum::Null),
                    Some(other) => Err(invalid(&other)),
                }
            }
        }
    }
}

fn float_argument(function: Function, value: &Datum) -> Result<f64, Error> {
    match value {
        Datum::Integer(value) => Ok(*value as f64),
        Datum::Float(value) => Ok(*value),
        other => Err(invalid_value(function, other)),
    }
}

/// `text` in the Unicode normalization form `form`.
pub(crate) fn normalized(text: &str, form: NormalForm) -> String {
    match form {
        NormalForm::Nfc => text.nfc().collect(),
        NormalForm::Nfd => text.nfd().collect(),
        NormalForm::Nfkc => text.nfkc().collect(),
        NormalForm::Nfkd => text.nfkd().collect(),
    }
}

fn string_argument(function: Function, value: Datum) -> Result<String, Error> {
    match value {
        Datum::String(text) => Ok(text),
        other => Err(invalid_value(function, &other)),
    }
}

fn integer_argument(function: Function, value: Option<Datum>) -> Result<Option<i64>, Error> {
    match value {
        Some(Datum::Integer(value)) => Ok(Some(value)),
        Some(Datum::Null) | None => Ok(None),
        Some(other) => Err(invalid_value(function, &other)),
    }
}

fn non_negative(value: i64) -> Result<usize, Error> {
    usize::try_from(value).map_err(|_| {
        Error::new(
            ErrorKind::NumberOutOfRange,
            "a length or position cannot be negative",
        )
    })
}

fn round_half_up(value: f64) -> f64 {
    let rounded = value.round();
    if value < 0.0 && rounded - value == -0.5 {
        rounded + 1.0
    } else {
        rounded
    }
}

fn range(start: Datum, end: Option<Datum>, step: Option<Datum>) -> Result<Datum, Error> {
    let integer = |value: Option<Datum>| match value {
        Some(Datum::Integer(value)) => Ok(Some(value)),
        Some(Datum::Null) => Ok(None),
        Some(other) => Err(invalid_value(Function::Range, &other)),
        None => Ok(Some(1)),
    };
    let start = match start {
        Datum::Integer(value) => value,
        other => return Err(invalid_value(Function::Range, &other)),
    };
    let (Some(end), Some(step)) = (integer(end)?, integer(step)?) else {
        return Ok(Datum::Null);
    };
    if step == 0 {
        return Err(Error::new(
            ErrorKind::NumberOutOfRange,
            "range() needs a step other than zero",
        ));
    }
    let distance = if step > 0 {
        i128::from(end) - i128::from(start)
    } else {
        i128::from(start) - i128::from(end)
    };
    let length = if distance < 0 {
        0
    } else {
        distance / i128::from(step).abs() + 1
    };
    if length > MAX_RANGE_LENGTH {
        return Err(Error::new(
            ErrorKind::NumberOutOfRange,
            format!("range() would build more than {MAX_RANGE_LENGTH} elements"),
        ));
    }
    let items = (0..length)
        .map_while(|index| i64::try_from(i128::from(start) + index * i128::from(step)).ok())
        .map(Datum::Integer)
        .collect();
    Ok(Datum::List(Arc::new(items)))
}

fn to_integer(value: Datum) -> Result<Datum, Error> {
    match value {
        Datum::Integer(value) => Ok(Datum::Integer(value)),
        Datum::Float(value) => Ok(float_to_integer(value)),
        Datum::Boolean(value) => Ok(Datum::Integer(i64::from(value))),
        Datum::String(text) => Ok(match number_in(&text) {
            Datum::Float(value) => whole_part(&text).unwrap_or_else(|| float_to_integer(value)),
            other => other,
        }),
        other => Err(invalid_value(Function::ToInteger, &other)),
    }
}

fn whole_part(text: &str) -> Option<Datum> {
    let text = text.trim();
    if text.contains(['e', 'E']) {
        return None;
    }
    let whole = text.split_once('.').map_or(text, |(whole, _)| whole);
    if whole.trim_start_matches(['+', '-']).is_empty() {
        return Some(Datum::Integer(0));
    }
    Some(whole.parse::<i64>().map_or(Datum::Null, Datum::Integer))
}

fn float_to_integer(value: f64) -> Datum {
    let truncated = value.trunc();
    if truncated.is_finite()
        && (-9.223_372_036_854_776e18..9.223_372_036_854_776e18).contains(&truncated)
    {
        Datum::Integer(truncated as i64)
    } else {
        Datum::Null
    }
}

fn to_float(value: Datum) -> Result<Datum, Error> {
    match value {
        Datum::Integer(value) => Ok(Datum::Float(value as f64)),
        Datum::Float(value) => Ok(Datum::Float(value)),
        Datum::String(text) => Ok(match number_in(&text) {
            Datum::Integer(value) => Datum::Float(value as f64),
            other => other,
        }),
        other => Err(invalid_value(Function::ToFloat, &other)),
    }
}

fn number_in(text: &str) -> Datum {
    let text = text.trim();
    if !is_decimal_number(text.strip_prefix(['+', '-']).unwrap_or(text)) {
        return Datum::Null;
    }
    if let Ok(value) = text.parse::<i64>() {
        return Datum::Integer(value);
    }
    match text.parse::<f64>() {
        Ok(value) if value.is_finite() => Datum::Float(value),
        _ => Datum::Null,
    }
}

fn is_decimal_number(unsigned: &str) -> bool {
    let digits = |part: &str| part.bytes().all(|byte| byte.is_ascii_digit());
    let (mantissa, exponent) = match unsigned.split_once(['e', 'E']) {
        Some((mantissa, exponent)) => (mantissa, Some(exponent)),
        None => (unsigned, None),
    };
    let (whole, fraction) = match mantissa.split_once('.') {
        Some((whole, fraction)) => (whole, Some(fraction)),
        None => (mantissa, None),
    };
    let fraction_is_digits = fraction.is_none_or(|part| !part.is_empty() && digits(part));
    let exponent_is_digits = exponent.is_none_or(|part| {
        let part = part.strip_prefix(['+', '-']).unwrap_or(part);
        !part.is_empty() && digits(part)
    });
    digits(whole)
        && fraction_is_digits
        && exponent_is_digits
        && (!whole.is_empty() || fraction.is_some())
}

fn to_string(value: Datum) -> Result<Datum, Error> {
    match value {
        Datum::String(text) => Ok(Datum::String(text)),
        Datum::Integer(value) => Ok(Datum::String(value.to_string())),
        Datum::Float(value) => Ok(Datum::String(float_text(value))),
        Datum::Boolean(value) => Ok(Datum::String(value.to_string())),
        Datum::Temporal(value) => Ok(Datum::String(value.to_string())),
        other => Err(invalid_value(Function::ToString, &other)),
    }
}

fn to_boolean(value: Datum) -> Result<Datum, Error> {
    match value {
        Datum::Boolean(value) => Ok(Datum::Boolean(value)),
        Datum::Integer(value) => Ok(Datum::Boolean(value != 0)),
        Datum::String(text) => Ok(match text.trim().to_ascii_lowercase().as_str() {
            "true" => Datum::Boolean(true),
            "false" => Datum::Boolean(false),
            _ => Datum::Null,
        }),
        other => Err(invalid_value(Function::ToBoolean, &other)),
    }
}
