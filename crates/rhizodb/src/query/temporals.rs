use crate::error::Error;
use crate::error::ErrorKind;
use crate::gql::ast::BinaryOp;
use crate::storage::ReadTransaction;
use crate::temporal;
use crate::temporal::Temporal;
use crate::temporal::arithmetic;
use crate::temporal::arithmetic::Span;
use crate::temporal::build;
use crate::temporal::build::Component;
use crate::temporal::build::Input;
use crate::temporal::build::Kind;
use crate::temporal::build::TemporalError;

use super::datum::Datum;
use super::eval::Env;

/// Which clock a function that reads the current time reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Moment {
    Transaction,
    Statement,
    Realtime,
}

/// A function that builds, reads, or measures temporal values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum TemporalFunction {
    Construct(Kind),
    Clock(Kind, Moment),
    Truncate(Kind),
    FromEpoch,
    FromEpochMillis,
    Duration,
    Between(Span),
}

/// The error for a temporal value that cannot be built or read.
pub(crate) fn temporal_error(error: TemporalError) -> Error {
    Error::new(ErrorKind::InvalidTemporalValue, error.to_string())
}

fn out_of_range() -> Error {
    Error::new(
        ErrorKind::InvalidTemporalValue,
        "the temporal result is out of the supported range",
    )
}

fn input(key: &str, value: &Datum) -> Result<Option<Input>, Error> {
    Ok(Some(match value {
        Datum::Null => return Ok(None),
        Datum::Integer(value) => Input::Integer(*value),
        Datum::Float(value) => Input::Float(*value),
        Datum::String(value) => Input::Text(value.clone()),
        Datum::Temporal(value) => Input::Temporal(value.clone()),
        other => {
            return Err(Error::new(
                ErrorKind::InvalidTemporalValue,
                format!("'{key}' cannot be a {}", other.type_name()),
            ));
        }
    }))
}

fn fields(value: &Datum) -> Result<Vec<(String, Input)>, Error> {
    let Datum::Map(entries) = value else {
        return Ok(Vec::new());
    };
    let mut fields = Vec::new();
    for (key, value) in entries.iter() {
        if let Some(value) = input(key, value)? {
            fields.push((key.to_string(), value));
        }
    }
    Ok(fields)
}

fn temporal(value: Temporal) -> Datum {
    Datum::Temporal(value)
}

impl<T: ReadTransaction> Env<'_, T> {
    /// Calls a temporal function on arguments the caller has checked against its signature.
    pub(crate) fn call_temporal(
        &self,
        function: TemporalFunction,
        args: Vec<Datum>,
    ) -> Result<Datum, Error> {
        if let TemporalFunction::Construct(kind) | TemporalFunction::Clock(kind, _) = function
            && args.is_empty()
        {
            return self.clock(function, kind, None);
        }
        if args.iter().any(Datum::is_null) && !matches!(function, TemporalFunction::Truncate(_)) {
            return Ok(Datum::Null);
        }
        let mut args = args.into_iter();
        let first = args.next().unwrap_or(Datum::Null);
        let second = args.next().unwrap_or(Datum::Null);
        let third = args.next().unwrap_or(Datum::Null);
        let value = match function {
            TemporalFunction::Construct(kind) => match &first {
                Datum::String(text) => build::parsed(kind, text),
                Datum::Map(_) => build::from_fields(kind, &fields(&first)?, &self.now.statement),
                Datum::Temporal(value) => build::converted(kind, value, &self.now.statement),
                _ => return Ok(Datum::Null),
            },
            TemporalFunction::Clock(kind, _) => match &first {
                Datum::String(text) => return self.clock(function, kind, Some(text)),
                _ => return Ok(Datum::Null),
            },
            TemporalFunction::Truncate(kind) => {
                let (Datum::String(unit), Datum::Temporal(value)) = (&first, &second) else {
                    return Ok(Datum::Null);
                };
                build::truncated(kind, unit, value, &fields(&third)?, &self.now.statement)
            }
            TemporalFunction::FromEpoch => match (&first, &second) {
                (Datum::Integer(seconds), Datum::Integer(nanoseconds)) => {
                    build::from_epoch(*seconds, *nanoseconds)
                }
                _ => return Ok(Datum::Null),
            },
            TemporalFunction::FromEpochMillis => match &first {
                Datum::Integer(millis) => build::from_epoch_millis(*millis),
                _ => return Ok(Datum::Null),
            },
            TemporalFunction::Duration => {
                let duration = match &first {
                    Datum::String(text) => build::parsed_duration(text),
                    Datum::Map(_) => build::duration_from_fields(&fields(&first)?),
                    _ => return Ok(Datum::Null),
                };
                duration.map(Temporal::Duration)
            }
            TemporalFunction::Between(span) => {
                let (Datum::Temporal(from), Datum::Temporal(to)) = (&first, &second) else {
                    return Ok(Datum::Null);
                };
                let duration = arithmetic::between(from, to, span).ok_or_else(out_of_range)?;
                Ok(Temporal::Duration(duration))
            }
        };
        value.map(temporal).map_err(temporal_error)
    }

    fn clock(
        &self,
        function: TemporalFunction,
        kind: Kind,
        zone: Option<&str>,
    ) -> Result<Datum, Error> {
        let realtime;
        let now = match function {
            TemporalFunction::Clock(_, Moment::Realtime) => {
                realtime = temporal::now();
                &realtime
            }
            TemporalFunction::Clock(_, Moment::Transaction) => &self.now.transaction,
            _ => &self.now.statement,
        };
        build::current(kind, zone, now)
            .map(temporal)
            .map_err(temporal_error)
    }
}

/// The value of the component `key` of a temporal value, read as a property.
pub(crate) fn component(value: &Temporal, key: &str) -> Result<Datum, Error> {
    match build::component(value, key) {
        Some(Component::Integer(value)) => Ok(Datum::Integer(value)),
        Some(Component::Text(value)) => Ok(Datum::String(value)),
        Some(Component::Overflow) => Err(Error::new(
            ErrorKind::IntegerOverflow,
            format!("the component `{key}` does not fit in an integer"),
        )),
        None => Err(Error::new(
            ErrorKind::InvalidTemporalValue,
            format!("a {} has no component `{key}`", value.type_name()),
        )),
    }
}

/// `lhs op rhs` where an operand is a temporal value, or `None` when the operator does not take
/// these operands.
pub(crate) fn arithmetic(op: BinaryOp, lhs: &Datum, rhs: &Datum) -> Option<Result<Datum, Error>> {
    let result = match (op, lhs, rhs) {
        (BinaryOp::Add, Datum::Temporal(value), Datum::Temporal(Temporal::Duration(duration)))
        | (BinaryOp::Add, Datum::Temporal(Temporal::Duration(duration)), Datum::Temporal(value)) => {
            arithmetic::plus(value, duration)
        }
        (
            BinaryOp::Subtract,
            Datum::Temporal(value),
            Datum::Temporal(Temporal::Duration(duration)),
        ) => arithmetic::negated(duration).and_then(|negated| arithmetic::plus(value, &negated)),
        (BinaryOp::Multiply, Datum::Temporal(Temporal::Duration(duration)), number)
        | (BinaryOp::Multiply, number, Datum::Temporal(Temporal::Duration(duration))) => {
            match number {
                Datum::Integer(factor) => arithmetic::times_integer(duration, *factor),
                Datum::Float(factor) => arithmetic::times_float(duration, *factor),
                _ => return None,
            }
            .map(Temporal::Duration)
        }
        (BinaryOp::Divide, Datum::Temporal(Temporal::Duration(duration)), divisor) => match divisor
        {
            Datum::Integer(0) => return Some(Err(division_by_zero())),
            Datum::Integer(divisor) => arithmetic::divided_by_integer(duration, *divisor),
            Datum::Float(divisor) if *divisor == 0.0 => {
                return Some(Err(division_by_zero()));
            }
            Datum::Float(divisor) => arithmetic::divided_by_float(duration, *divisor),
            _ => return None,
        }
        .map(Temporal::Duration),
        _ => return None,
    };
    Some(result.map(Datum::Temporal).ok_or_else(out_of_range))
}

fn division_by_zero() -> Error {
    Error::new(ErrorKind::DivisionByZero, "a duration divided by zero")
}
