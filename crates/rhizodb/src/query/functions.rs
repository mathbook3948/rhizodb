use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;

use crate::error::Error;
use crate::error::ErrorKind;
use crate::storage::ReadTransaction;

use super::datum::Datum;
use super::eval::Env;
use super::eval::float_text;
use super::shape::INSTANT;
use super::shape::NUMBER;
use super::shape::Shape;
use super::shape::admits;
use super::temporals::Moment;
use super::temporals::TemporalFunction;
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

const SIGNATURES: [(&str, Function, usize, usize, &[Accepted]); 85] = [
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
    ("floor", Function::Floor, 1, 1, &[NUMBER]),
    ("round", Function::Round, 1, 1, &[NUMBER]),
    ("sqrt", Function::Sqrt, 1, 1, &[NUMBER]),
    ("exp", Function::Exp, 1, 1, &[NUMBER]),
    ("log", Function::Log, 1, 1, &[NUMBER]),
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
    ("tolower", Function::ToLower, 1, 1, &[STRING]),
    ("trim", Function::Trim, 1, 1, &[STRING]),
    ("ltrim", Function::LTrim, 1, 1, &[STRING]),
    ("rtrim", Function::RTrim, 1, 1, &[STRING]),
    ("substring", Function::Substring, 2, 3, &[STRING, INTEGER]),
    ("replace", Function::Replace, 3, 3, &[STRING]),
    ("split", Function::Split, 2, 2, &[STRING]),
    ("left", Function::Left, 2, 2, &[STRING, INTEGER]),
    ("right", Function::Right, 2, 2, &[STRING, INTEGER]),
    ("exists", Function::Exists, 1, 1, &[ANY]),
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

    /// The error kind for an argument at `index` of a type the function does not take, when it
    /// differs from the kind a type error has elsewhere: a parameter that takes only integers
    /// raises [`ErrorKind::NonIntegerArgument`].
    pub(crate) fn argument_error(self, index: usize) -> Option<ErrorKind> {
        (self.parameter(index) == INTEGER).then_some(ErrorKind::NonIntegerArgument)
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
                return Err(match function.argument_error(index) {
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
            Function::Pi | Function::E | Function::Rand | Function::Exists | Function::Temporal(_)
        ) && first.is_null()
        {
            return Ok(Datum::Null);
        }
        let invalid = |value: &Datum| invalid_value(function, value);
        match function {
            Function::Coalesce => Ok(first),
            Function::Exists => Ok(Datum::Boolean(!first.is_null())),
            Function::Pi => Ok(Datum::Float(std::f64::consts::PI)),
            Function::E => Ok(Datum::Float(std::f64::consts::E)),
            Function::Rand => Ok(Datum::Float(random_unit())),
            Function::Id => match first {
                Datum::Node(id) => Ok(Datum::Integer(id.0 as i64)),
                Datum::Relationship(id) => Ok(Datum::Integer(id.0 as i64)),
                other => Err(invalid(&other)),
            },
            Function::Labels => match first {
                Datum::Node(id) => Ok(Datum::List(
                    self.label_names(&self.node(id)?)?
                        .into_iter()
                        .map(Datum::String)
                        .collect(),
                )),
                other => Err(invalid(&other)),
            },
            Function::Type => match first {
                Datum::Relationship(id) => Ok(Datum::String(
                    self.tx.token_name(self.relationship(id)?.rel_type)?,
                )),
                other => Err(invalid(&other)),
            },
            Function::Keys | Function::Properties => {
                let entries = match first {
                    Datum::Node(id) => self.property_map(&self.node(id)?.properties)?,
                    Datum::Relationship(id) => {
                        self.property_map(&self.relationship(id)?.properties)?
                    }
                    Datum::Map(entries) => entries,
                    other => return Err(invalid(&other)),
                };
                Ok(if function == Function::Keys {
                    Datum::List(entries.into_keys().map(Datum::String).collect())
                } else {
                    Datum::Map(entries)
                })
            }
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
                Datum::List(mut items) => Ok(match function {
                    Function::Head => items.into_iter().next().unwrap_or(Datum::Null),
                    Function::Last => items.pop().unwrap_or(Datum::Null),
                    _ => Datum::List(items.into_iter().skip(1).collect()),
                }),
                other => Err(invalid(&other)),
            },
            Function::Reverse => match first {
                Datum::List(mut items) => {
                    items.reverse();
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
            | Function::Sin
            | Function::Cos
            | Function::Tan
            | Function::Asin
            | Function::Acos
            | Function::Atan
            | Function::Degrees
            | Function::Radians => {
                let value = float_argument(function, &first)?;
                Ok(Datum::Float(match function {
                    Function::Ceil => value.ceil(),
                    Function::Floor => value.floor(),
                    Function::Round => round_half_up(value),
                    Function::Sqrt => value.sqrt(),
                    Function::Exp => value.exp(),
                    Function::Log => value.ln(),
                    Function::Log10 => value.log10(),
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
                Datum::Path(path) => Ok(Datum::List(
                    path.nodes.into_iter().map(Datum::Node).collect(),
                )),
                other => Err(invalid(&other)),
            },
            Function::Relationships => match first {
                Datum::Path(path) => Ok(Datum::List(
                    path.relationships
                        .into_iter()
                        .map(Datum::Relationship)
                        .collect(),
                )),
                other => Err(invalid(&other)),
            },
            Function::ToUpper
            | Function::ToLower
            | Function::Trim
            | Function::LTrim
            | Function::RTrim => match first {
                Datum::String(text) => Ok(Datum::String(match function {
                    Function::ToUpper => text.to_uppercase(),
                    Function::ToLower => text.to_lowercase(),
                    Function::Trim => text.trim().to_owned(),
                    Function::LTrim => text.trim_start().to_owned(),
                    _ => text.trim_end().to_owned(),
                })),
                other => Err(invalid(&other)),
            },
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
                Ok(Datum::String(text.replace(from.as_str(), to)))
            }
            Function::Temporal(temporal) => {
                let args = std::iter::once(first).chain(second).chain(third);
                self.call_temporal(temporal, args.take(count).collect())
            }
            Function::Split => {
                let text = string_argument(function, first)?;
                match second {
                    Some(Datum::String(delimiter)) if delimiter.is_empty() => Ok(Datum::List(
                        text.chars()
                            .map(|char| Datum::String(char.to_string()))
                            .collect(),
                    )),
                    Some(Datum::String(delimiter)) => Ok(Datum::List(
                        text.split(delimiter.as_str())
                            .map(|part| Datum::String(part.to_owned()))
                            .collect(),
                    )),
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
    Ok(Datum::List(items))
}

fn to_integer(value: Datum) -> Result<Datum, Error> {
    match value {
        Datum::Integer(value) => Ok(Datum::Integer(value)),
        Datum::Float(value) => Ok(float_to_integer(value)),
        Datum::Boolean(value) => Ok(Datum::Integer(i64::from(value))),
        Datum::String(text) => Ok(match number_in(&text) {
            Datum::Float(value) => float_to_integer(value),
            other => other,
        }),
        other => Err(invalid_value(Function::ToInteger, &other)),
    }
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
