use crate::temporal::Temporal;
use crate::temporal::Time;
use crate::value::PropertyValue;

const LIST_END: u8 = 0x00;
const BOOLEAN: u8 = 0x10;
const NUMBER: u8 = 0x20;
const STRING: u8 = 0x30;
const BYTES: u8 = 0x40;
const LIST: u8 = 0x50;
const DATE: u8 = 0x60;
const LOCAL_TIME: u8 = 0x61;
const TIME: u8 = 0x62;
const LOCAL_DATETIME: u8 = 0x63;
const DATETIME: u8 = 0x64;
const DURATION: u8 = 0x65;

const BELOW_INTEGERS: u8 = 1;
const AMONG_INTEGERS: u8 = 2;
const ABOVE_INTEGERS: u8 = 3;

const ESCAPE: u8 = 0x00;
const ESCAPED_ZERO: u8 = 0xFF;
const TEXT_END: u8 = 0x01;

const NO_ZONE: u8 = 0;
const NAMED_ZONE: u8 = 1;

const TWO_TO_63: f64 = 9_223_372_036_854_775_808.0;

/// The keys of one index whose bytes lie from `start` up to, but not including, `end`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct KeyRange {
    pub(crate) start: Vec<u8>,
    pub(crate) end: Vec<u8>,
}

impl KeyRange {
    fn prefixed(prefix: Vec<u8>) -> Self {
        let end = successor(&prefix);
        Self { start: prefix, end }
    }

    /// The keys of the values equal to `value`, or `None` when no stored value equals it.
    pub(crate) fn equal_to(value: &PropertyValue) -> Option<Self> {
        key(value).map(Self::prefixed)
    }

    /// The keys both ranges hold, or `None` when they share none.
    pub(crate) fn intersect(&self, other: &Self) -> Option<Self> {
        let start = self.start.clone().max(other.start.clone());
        let end = self.end.clone().min(other.end.clone());
        (start < end).then_some(Self { start, end })
    }
}

/// Which side of a bound the wanted values lie on, as in `stored < bound`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Bound {
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
}

/// The stored values that compare to a bound as a [`Bound`] asks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Compared {
    /// The values whose keys lie in the range.
    Range(KeyRange),
    /// No stored value compares to the bound, so the comparison is never true.
    Never,
    /// The comparison has no range of keys, and every element has to be read.
    Unserved,
}

/// The values of every stored property value that `bound` compares with under `<` and its
/// siblings, and the part of each key that the comparison reads.
pub(crate) fn compared(side: Bound, bound: &PropertyValue) -> Compared {
    let Some((family, prefix)) = comparison_prefix(bound) else {
        return match bound {
            PropertyValue::List(_) => Compared::Unserved,
            _ => Compared::Never,
        };
    };
    let first = vec![family];
    let past = vec![family + 1];
    let range = match side {
        Bound::Less => KeyRange {
            start: first,
            end: prefix,
        },
        Bound::LessEqual => KeyRange {
            start: first,
            end: successor(&prefix),
        },
        Bound::Greater => KeyRange {
            start: successor(&prefix),
            end: past,
        },
        Bound::GreaterEqual => KeyRange {
            start: prefix,
            end: past,
        },
    };
    if range.start < range.end {
        Compared::Range(range)
    } else {
        Compared::Never
    }
}

fn comparison_prefix(value: &PropertyValue) -> Option<(u8, Vec<u8>)> {
    match value {
        PropertyValue::Float(number) if number.is_nan() => None,
        PropertyValue::Bytes(_) | PropertyValue::List(_) => None,
        PropertyValue::Temporal(Temporal::Duration(_)) => None,
        PropertyValue::Temporal(Temporal::Time(time)) => {
            let mut out = vec![TIME];
            put_i64(&mut out, utc_nanos(time));
            Some((TIME, out))
        }
        PropertyValue::Temporal(Temporal::DateTime(datetime)) => {
            let mut out = vec![DATETIME];
            put_i64(&mut out, datetime.epoch_second());
            out.extend_from_slice(&datetime.nanosecond().to_be_bytes());
            Some((DATETIME, out))
        }
        other => {
            let out = key(other)?;
            Some((*out.first()?, out))
        }
    }
}

/// The key of a stored value, or `None` for a value that equals nothing: NaN, or a list that
/// holds one. Two values have the same key exactly when `=` holds between them, and keys of
/// values that `<` orders sort in that order.
pub(crate) fn key(value: &PropertyValue) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    encode(value, &mut out).then_some(out)
}

fn encode(value: &PropertyValue, out: &mut Vec<u8>) -> bool {
    match value {
        PropertyValue::Boolean(value) => {
            out.push(BOOLEAN);
            out.push(u8::from(*value));
        }
        PropertyValue::Integer(value) => {
            out.push(NUMBER);
            out.push(AMONG_INTEGERS);
            put_i64(out, *value);
            put_fraction(out, 0.0);
        }
        PropertyValue::Float(value) => {
            if value.is_nan() {
                return false;
            }
            out.push(NUMBER);
            encode_float(*value, out);
        }
        PropertyValue::String(text) => {
            out.push(STRING);
            put_text(out, text.as_bytes());
        }
        PropertyValue::Bytes(bytes) => {
            out.push(BYTES);
            put_text(out, bytes);
        }
        PropertyValue::List(items) => {
            out.push(LIST);
            for item in items {
                if !encode(item, out) {
                    return false;
                }
            }
            out.push(LIST_END);
        }
        PropertyValue::Temporal(temporal) => encode_temporal(temporal, out),
    }
    true
}

fn encode_float(value: f64, out: &mut Vec<u8>) {
    if value < -TWO_TO_63 {
        out.push(BELOW_INTEGERS);
        out.extend_from_slice(&(!value.to_bits()).to_be_bytes());
    } else if value >= TWO_TO_63 {
        out.push(ABOVE_INTEGERS);
        out.extend_from_slice(&value.to_bits().to_be_bytes());
    } else {
        let whole = value.trunc();
        out.push(AMONG_INTEGERS);
        put_i64(out, whole as i64);
        put_fraction(out, value - whole);
    }
}

fn encode_temporal(value: &Temporal, out: &mut Vec<u8>) {
    match value {
        Temporal::Date(date) => {
            out.push(DATE);
            put_i64(out, date.days());
        }
        Temporal::LocalTime(time) => {
            out.push(LOCAL_TIME);
            put_i64(out, time.nanos());
        }
        Temporal::Time(time) => {
            out.push(TIME);
            put_i64(out, utc_nanos(time));
            put_i32(out, time.offset_seconds());
        }
        Temporal::LocalDateTime(local) => {
            out.push(LOCAL_DATETIME);
            put_i64(out, local.date().days());
            put_i64(out, local.time().nanos());
        }
        Temporal::DateTime(datetime) => {
            out.push(DATETIME);
            put_i64(out, datetime.epoch_second());
            out.extend_from_slice(&datetime.nanosecond().to_be_bytes());
            put_i32(out, datetime.offset_seconds());
            match datetime.zone_name() {
                None => out.push(NO_ZONE),
                Some(zone) => {
                    out.push(NAMED_ZONE);
                    put_text(out, zone.as_bytes());
                }
            }
        }
        Temporal::Duration(duration) => {
            out.push(DURATION);
            put_i64(out, duration.months());
            put_i64(out, duration.days());
            put_i64(out, duration.seconds());
            out.extend_from_slice(&duration.nanoseconds().to_be_bytes());
        }
    }
}

fn utc_nanos(time: &Time) -> i64 {
    time.local_time().nanos() - i64::from(time.offset_seconds()) * 1_000_000_000
}

fn put_i64(out: &mut Vec<u8>, value: i64) {
    out.extend_from_slice(&((value as u64) ^ (1 << 63)).to_be_bytes());
}

fn put_fraction(out: &mut Vec<u8>, fraction: f64) {
    let bits = fraction.to_bits();
    let ordered = if bits >> 63 == 1 {
        !bits
    } else {
        bits | (1 << 63)
    };
    out.extend_from_slice(&ordered.to_be_bytes());
}

fn put_i32(out: &mut Vec<u8>, value: i32) {
    out.extend_from_slice(&((value as u32) ^ (1 << 31)).to_be_bytes());
}

fn put_text(out: &mut Vec<u8>, bytes: &[u8]) {
    for byte in bytes {
        out.push(*byte);
        if *byte == ESCAPE {
            out.push(ESCAPED_ZERO);
        }
    }
    out.push(ESCAPE);
    out.push(TEXT_END);
}

fn successor(prefix: &[u8]) -> Vec<u8> {
    let mut out = prefix.to_vec();
    while let Some(last) = out.pop() {
        if last < u8::MAX {
            out.push(last + 1);
            break;
        }
    }
    out
}
