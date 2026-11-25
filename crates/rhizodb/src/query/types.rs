use std::collections::BTreeSet;

use crate::error::Error;
use crate::error::ErrorKind;
use crate::gql::ast::TypeExpr;
use crate::gql::ast::TypeMember;
use crate::gql::ast::ValueType;
use crate::storage::ReadTransaction;
use crate::temporal::Temporal;
use crate::temporal::build::Kind;

use super::datum;
use super::datum::Datum;
use super::eval::Env;
use super::functions::Function;
use super::temporals::TemporalFunction;

/// Whether `value` belongs to one of the types of `expr`, where null belongs to every type not
/// declared `NOT NULL` except `NOTHING`.
pub(crate) fn is_typed(value: &Datum, expr: &TypeExpr) -> bool {
    expr.members
        .iter()
        .any(|member| member_holds(value, member))
}

fn member_holds(value: &Datum, member: &TypeMember) -> bool {
    match (value, &member.value_type) {
        (_, ValueType::Nothing) => false,
        (Datum::Null, _) => !member.not_null,
        (_, ValueType::Null) => false,
        (_, ValueType::Any) => true,
        (Datum::List(items), ValueType::List(inner)) => {
            items.iter().all(|item| is_typed(item, inner))
        }
        (value, value_type) => base_type(value).as_ref() == Some(value_type),
    }
}

fn base_type(value: &Datum) -> Option<ValueType> {
    Some(match value {
        Datum::Null | Datum::List(_) => return None,
        Datum::Boolean(_) => ValueType::Boolean,
        Datum::Integer(_) => ValueType::Integer,
        Datum::Float(_) => ValueType::Float,
        Datum::String(_) => ValueType::String,
        Datum::Bytes(_) => ValueType::Bytes,
        Datum::Map(_) => ValueType::Map,
        Datum::Node(_) => ValueType::Node,
        Datum::Relationship(_) => ValueType::Relationship,
        Datum::Path(_) => ValueType::Path,
        Datum::Temporal(temporal) => match temporal {
            Temporal::Date(_) => ValueType::Date,
            Temporal::LocalTime(_) => ValueType::LocalTime,
            Temporal::Time(_) => ValueType::ZonedTime,
            Temporal::LocalDateTime(_) => ValueType::LocalDateTime,
            Temporal::DateTime(_) => ValueType::ZonedDateTime,
            Temporal::Duration(_) => ValueType::Duration,
        },
    })
}

fn rank(name: &str) -> usize {
    const ORDER: [&str; 15] = [
        "BOOLEAN",
        "STRING",
        "INTEGER",
        "FLOAT",
        "DATE",
        "LOCAL TIME",
        "ZONED TIME",
        "LOCAL DATETIME",
        "ZONED DATETIME",
        "DURATION",
        "BYTES",
        "NODE",
        "RELATIONSHIP",
        "MAP",
        "PATH",
    ];
    ORDER
        .iter()
        .position(|held| *held == name)
        .unwrap_or(ORDER.len())
}

fn type_word(value_type: &ValueType) -> &'static str {
    match value_type {
        ValueType::Any => "ANY",
        ValueType::Nothing => "NOTHING",
        ValueType::Null => "NULL",
        ValueType::Boolean => "BOOLEAN",
        ValueType::String => "STRING",
        ValueType::Integer => "INTEGER",
        ValueType::Float => "FLOAT",
        ValueType::Date => "DATE",
        ValueType::LocalTime => "LOCAL TIME",
        ValueType::ZonedTime => "ZONED TIME",
        ValueType::LocalDateTime => "LOCAL DATETIME",
        ValueType::ZonedDateTime => "ZONED DATETIME",
        ValueType::Duration => "DURATION",
        ValueType::Bytes => "BYTES",
        ValueType::Map => "MAP",
        ValueType::Node => "NODE",
        ValueType::Relationship => "RELATIONSHIP",
        ValueType::Path => "PATH",
        ValueType::List(_) => "LIST",
    }
}

/// The name of the most specific type of `value`, as `valueType()` gives it.
pub(crate) fn type_name(value: &Datum) -> String {
    match value {
        Datum::Null => "NULL".to_owned(),
        value => format!("{} NOT NULL", bare_name(value)),
    }
}

fn bare_name(value: &Datum) -> String {
    let Datum::List(items) = value else {
        return base_type(value)
            .map_or("NULL", |value_type| type_word(&value_type))
            .to_owned();
    };
    let mut nullable = false;
    let mut names = BTreeSet::new();
    for item in items.iter() {
        match item {
            Datum::Null => nullable = true,
            item => {
                let name = bare_name(item);
                names.insert((rank(&name), name));
            }
        }
    }
    let inner = if names.is_empty() {
        if nullable { "NULL" } else { "NOTHING" }.to_owned()
    } else {
        names
            .into_iter()
            .map(|(_, name)| {
                if nullable {
                    name
                } else {
                    format!("{name} NOT NULL")
                }
            })
            .collect::<Vec<_>>()
            .join(" | ")
    };
    format!("LIST<{inner}>")
}

fn invalid_cast(value: &Datum, target: &ValueType) -> Error {
    Error::new(
        ErrorKind::InvalidCast,
        format!(
            "a {} cannot be cast to {}",
            value.type_name(),
            type_word(target)
        ),
    )
}

impl<T: ReadTransaction> Env<'_, T> {
    /// `CAST(value AS target)`: null stays null, a value of the target type stays as it is, and
    /// any other value converts or fails with `ErrorKind::InvalidCast`.
    pub(crate) fn cast(&self, value: Datum, target: &ValueType) -> Result<Datum, Error> {
        if value.is_null()
            || member_holds(
                &value,
                &TypeMember {
                    value_type: target.clone(),
                    not_null: false,
                },
            )
        {
            return Ok(value);
        }
        let converted = match (target, &value) {
            (ValueType::List(inner), Datum::List(items)) => {
                let Some(TypeMember {
                    value_type: inner, ..
                }) = inner.members.first()
                else {
                    return Err(invalid_cast(&value, target));
                };
                let mut cast = Vec::with_capacity(items.len());
                for item in items.iter() {
                    cast.push(self.cast(item.clone(), inner)?);
                }
                return datum::list(cast);
            }
            (
                ValueType::String,
                Datum::Boolean(_) | Datum::Integer(_) | Datum::Float(_) | Datum::Temporal(_),
            ) => self.call(Function::ToString, vec![value.clone()])?,
            (ValueType::Integer, Datum::Float(_) | Datum::String(_)) => {
                self.call(Function::ToInteger, vec![value.clone()])?
            }
            (ValueType::Float, Datum::Integer(_) | Datum::String(_)) => {
                self.call(Function::ToFloat, vec![value.clone()])?
            }
            (ValueType::Boolean, Datum::String(_)) => {
                self.call(Function::ToBoolean, vec![value.clone()])?
            }
            (ValueType::Duration, Datum::String(_)) => self.call(
                Function::Temporal(TemporalFunction::Duration),
                vec![value.clone()],
            )?,
            (
                ValueType::Date
                | ValueType::LocalTime
                | ValueType::ZonedTime
                | ValueType::LocalDateTime
                | ValueType::ZonedDateTime,
                Datum::String(_) | Datum::Temporal(_),
            ) => {
                let kind = match target {
                    ValueType::Date => Kind::Date,
                    ValueType::LocalTime => Kind::LocalTime,
                    ValueType::ZonedTime => Kind::Time,
                    ValueType::LocalDateTime => Kind::LocalDateTime,
                    _ => Kind::DateTime,
                };
                if matches!(value, Datum::Temporal(Temporal::Duration(_))) {
                    return Err(invalid_cast(&value, target));
                }
                self.call(
                    Function::Temporal(TemporalFunction::Construct(kind)),
                    vec![value.clone()],
                )?
            }
            _ => Datum::Null,
        };
        if converted.is_null() {
            return Err(invalid_cast(&value, target));
        }
        Ok(converted)
    }
}
