use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::sync::Arc;

use crate::error::Error;
use crate::error::ErrorKind;

use super::datum::Datum;

/// A random version 4 UUID in its hyphenated lowercase form.
pub(crate) fn random_uuid() -> Datum {
    let state = RandomState::new();
    let now = std::time::SystemTime::now();
    let high = u128::from(state.hash_one((now, 0u8)));
    let low = u128::from(state.hash_one((now, 1u8)));
    let bits = ((high << 64) | low) & !(0xf << 76) & !(0x3 << 62) | (0x4 << 76) | (0x2 << 62);
    let hex = format!("{bits:032x}");
    Datum::String(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Up,
    Down,
    Ceiling,
    Floor,
    HalfUp,
    HalfDown,
    HalfEven,
}

fn mode_named(name: &str) -> Option<Mode> {
    [
        ("UP", Mode::Up),
        ("DOWN", Mode::Down),
        ("CEILING", Mode::Ceiling),
        ("FLOOR", Mode::Floor),
        ("HALF_UP", Mode::HalfUp),
        ("HALF_DOWN", Mode::HalfDown),
        ("HALF_EVEN", Mode::HalfEven),
    ]
    .into_iter()
    .find(|(held, _)| held.eq_ignore_ascii_case(name))
    .map(|(_, mode)| mode)
}

/// `round(value, precision, mode)`: the value rounded to `precision` decimal places of its
/// shortest decimal form, by `mode`, which is `HALF_UP` when left out.
pub(crate) fn round(value: f64, precision: i64, mode: Option<&str>) -> Result<Datum, Error> {
    let mode = match mode {
        None => Mode::HalfUp,
        Some(name) => mode_named(name).ok_or_else(|| {
            Error::new(
                ErrorKind::InvalidArgumentValue,
                format!("round() knows no rounding mode `{name}`"),
            )
        })?,
    };
    let Ok(places) = usize::try_from(precision) else {
        return Err(Error::new(
            ErrorKind::NumberOutOfRange,
            "round() cannot take a negative precision",
        ));
    };
    if !value.is_finite() {
        return Ok(Datum::Float(value));
    }
    let text = format!("{}", value.abs());
    let (whole, fraction) = text.split_once('.').unwrap_or((&text, ""));
    if fraction.len() <= places {
        return Ok(Datum::Float(value));
    }
    let (kept, dropped) = fraction.split_at(places);
    let mut digits: Vec<u8> = whole.bytes().chain(kept.bytes()).collect();
    let first = dropped.as_bytes().first().copied().unwrap_or(b'0');
    let beyond = dropped.bytes().skip(1).any(|digit| digit != b'0');
    let nonzero = first != b'0' || beyond;
    let last_odd = digits.last().is_some_and(|digit| (digit - b'0') % 2 == 1);
    let negative = value.is_sign_negative();
    let away = match mode {
        Mode::Up => nonzero,
        Mode::Down => false,
        Mode::Ceiling => nonzero && !negative,
        Mode::Floor => nonzero && negative,
        Mode::HalfUp => first >= b'5',
        Mode::HalfDown => first > b'5' || (first == b'5' && beyond),
        Mode::HalfEven => first > b'5' || (first == b'5' && (beyond || last_odd)),
    };
    if away {
        let mut carry = true;
        for digit in digits.iter_mut().rev() {
            if *digit == b'9' {
                *digit = b'0';
            } else {
                *digit += 1;
                carry = false;
                break;
            }
        }
        if carry {
            digits.insert(0, b'1');
        }
    }
    let split = digits.len() - places;
    let text = format!(
        "{}{}.{}",
        if negative { "-" } else { "" },
        String::from_utf8_lossy(&digits[..split]),
        String::from_utf8_lossy(&digits[split..])
    );
    Ok(Datum::Float(text.parse().unwrap_or(value)))
}

/// The numbers of a list for a vector function, or `None` when the list holds a null.
pub(crate) fn vector(name: &str, value: &Datum) -> Result<Option<Vec<f64>>, Error> {
    let Datum::List(items) = value else {
        return Err(Error::rejecting_type(
            ErrorKind::InvalidArgumentValue,
            format!(
                "{name}() takes lists of numbers, not a {}",
                value.type_name()
            ),
        ));
    };
    let mut numbers = Vec::with_capacity(items.len());
    for item in items.iter() {
        numbers.push(match item {
            Datum::Integer(value) => *value as f64,
            Datum::Float(value) => *value,
            Datum::Null => return Ok(None),
            other => {
                return Err(Error::rejecting_type(
                    ErrorKind::InvalidArgumentValue,
                    format!(
                        "{name}() takes lists of numbers, not of a {}",
                        other.type_name()
                    ),
                ));
            }
        });
    }
    Ok(Some(numbers))
}

/// Neo4j's vector similarities: cosine mapped to `[0, 1]` as `(1 + cos) / 2`, and euclidean as
/// `1 / (1 + d²)`. Vectors of different lengths fail, and a cosine of a zero vector is null.
pub(crate) fn similarity(name: &str, cosine: bool, a: &[f64], b: &[f64]) -> Result<Datum, Error> {
    if a.len() != b.len() || a.is_empty() {
        return Err(Error::new(
            ErrorKind::InvalidArgumentValue,
            format!(
                "{name}() takes two vectors of one nonzero length, not of {} and {}",
                a.len(),
                b.len()
            ),
        ));
    }
    if cosine {
        let dot: f64 = a.iter().zip(b).map(|(x, y)| x * y).sum();
        let norms = a.iter().map(|x| x * x).sum::<f64>().sqrt()
            * b.iter().map(|y| y * y).sum::<f64>().sqrt();
        if norms == 0.0 || !norms.is_finite() {
            return Ok(Datum::Null);
        }
        return Ok(Datum::Float((1.0 + (dot / norms).clamp(-1.0, 1.0)) / 2.0));
    }
    let squared: f64 = a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum();
    Ok(Datum::Float(1.0 / (1.0 + squared)))
}

/// Applies a conversion that gives null for what it cannot convert to every item of a list.
pub(crate) fn convert_list(
    name: &str,
    value: Datum,
    convert: impl Fn(Datum) -> Option<Datum>,
) -> Result<Datum, Error> {
    match value {
        Datum::List(items) => Ok(Datum::list(
            Arc::unwrap_or_clone(items)
                .into_iter()
                .map(|item| convert(item).unwrap_or(Datum::Null))
                .collect(),
        )),
        other => Err(Error::rejecting_type(
            ErrorKind::InvalidArgumentValue,
            format!("{name}() takes a list, not a {}", other.type_name()),
        )),
    }
}
