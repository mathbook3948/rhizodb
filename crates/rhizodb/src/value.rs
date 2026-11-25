use std::error::Error;
use std::fmt;

use crate::temporal::Date;
use crate::temporal::DateTime;
use crate::temporal::Duration;
use crate::temporal::LocalDateTime;
use crate::temporal::LocalTime;
use crate::temporal::Temporal;
use crate::temporal::Time;

pub(crate) const MAX_DEPTH: usize = 64;

pub(crate) const TAG_BOOLEAN: u8 = 0;
pub(crate) const TAG_INTEGER: u8 = 1;
pub(crate) const TAG_FLOAT: u8 = 2;
pub(crate) const TAG_STRING: u8 = 3;
const TAG_BYTES: u8 = 4;
const TAG_LIST: u8 = 5;
const TAG_DATE: u8 = 6;
const TAG_LOCAL_TIME: u8 = 7;
const TAG_TIME: u8 = 8;
const TAG_LOCAL_DATETIME: u8 = 9;
const TAG_OFFSET_DATETIME: u8 = 10;
const TAG_ZONED_DATETIME: u8 = 11;
const TAG_DURATION: u8 = 12;
const TAG_SINGLE_FLOATS: u8 = 13;
const TAG_VARINT_INTEGER: u8 = 14;

/// A stored property value. Null is never stored: setting a property to null removes it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum PropertyValue {
    Boolean(bool),
    Integer(i64),
    Float(f64),
    String(String),
    Bytes(Vec<u8>),
    List(Vec<PropertyValue>),
    Temporal(Temporal),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DecodeError {
    UnexpectedEnd,
    UnknownTag(u8),
    InvalidBoolean(u8),
    InvalidUtf8,
    LengthOverflow,
    NestingTooDeep,
    TrailingBytes,
    InvalidTemporal,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedEnd => write!(formatter, "input ended inside a value"),
            Self::UnknownTag(tag) => write!(formatter, "unknown value tag {tag}"),
            Self::InvalidBoolean(byte) => write!(formatter, "invalid boolean byte {byte}"),
            Self::InvalidUtf8 => write!(formatter, "string is not valid UTF-8"),
            Self::LengthOverflow => write!(formatter, "length does not fit in memory"),
            Self::NestingTooDeep => write!(formatter, "lists nest deeper than {MAX_DEPTH}"),
            Self::TrailingBytes => write!(formatter, "bytes remain after the value"),
            Self::InvalidTemporal => write!(formatter, "temporal value is out of range"),
        }
    }
}

impl Error for DecodeError {}

fn single_floats(items: &[PropertyValue]) -> Option<impl Iterator<Item = f32>> {
    let single = |item: &PropertyValue| match item {
        PropertyValue::Float(value) => {
            let narrow = *value as f32;
            (f64::from(narrow).to_bits() == value.to_bits()).then_some(narrow)
        }
        _ => None,
    };
    (!items.is_empty() && items.iter().all(|item| single(item).is_some()))
        .then(|| items.iter().filter_map(single))
}

impl PropertyValue {
    #[cfg(test)]
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode_into(&mut out);
        out
    }

    pub(crate) fn encode_into(&self, out: &mut Vec<u8>) {
        match self {
            Self::Boolean(value) => {
                out.push(TAG_BOOLEAN);
                out.push(u8::from(*value));
            }
            Self::Integer(value) => {
                out.push(TAG_INTEGER);
                out.extend_from_slice(&value.to_be_bytes());
            }
            Self::Float(value) => {
                out.push(TAG_FLOAT);
                out.extend_from_slice(&value.to_bits().to_be_bytes());
            }
            Self::String(value) => {
                out.push(TAG_STRING);
                put_varint(out, value.len());
                out.extend_from_slice(value.as_bytes());
            }
            Self::Bytes(value) => {
                out.push(TAG_BYTES);
                put_varint(out, value.len());
                out.extend_from_slice(value);
            }
            Self::List(items) => match single_floats(items) {
                Some(floats) => {
                    out.push(TAG_SINGLE_FLOATS);
                    put_varint(out, items.len());
                    for float in floats {
                        out.extend_from_slice(&float.to_bits().to_be_bytes());
                    }
                }
                None => {
                    out.push(TAG_LIST);
                    put_varint(out, items.len());
                    for item in items {
                        item.encode_into(out);
                    }
                }
            },
            Self::Temporal(value) => encode_temporal(value, out),
        }
    }

    /// The value as `encode_into` writes it, except that an integer whose zigzag varint fits in
    /// eight bytes takes that varint instead, so the value is never longer.
    pub(crate) fn encode_compact_into(&self, out: &mut Vec<u8>) {
        match self {
            Self::Integer(value) if zigzag(*value) < 1 << 56 => {
                out.push(TAG_VARINT_INTEGER);
                put_varint_u64(out, zigzag(*value));
            }
            other => other.encode_into(out),
        }
    }

    #[cfg(test)]
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let mut reader = Reader::new(bytes);
        let value = reader.value()?;
        reader.finish()?;
        Ok(value)
    }
}

fn encode_temporal(value: &Temporal, out: &mut Vec<u8>) {
    match value {
        Temporal::Date(date) => {
            out.push(TAG_DATE);
            out.extend_from_slice(&date.days().to_be_bytes());
        }
        Temporal::LocalTime(time) => {
            out.push(TAG_LOCAL_TIME);
            out.extend_from_slice(&time.nanos().to_be_bytes());
        }
        Temporal::Time(time) => {
            out.push(TAG_TIME);
            out.extend_from_slice(&time.local_time().nanos().to_be_bytes());
            out.extend_from_slice(&time.offset_seconds().to_be_bytes());
        }
        Temporal::LocalDateTime(local) => {
            out.push(TAG_LOCAL_DATETIME);
            out.extend_from_slice(&local.date().days().to_be_bytes());
            out.extend_from_slice(&local.time().nanos().to_be_bytes());
        }
        Temporal::DateTime(datetime) => {
            let zone = datetime.zone_name();
            out.push(if zone.is_some() {
                TAG_ZONED_DATETIME
            } else {
                TAG_OFFSET_DATETIME
            });
            out.extend_from_slice(&datetime.epoch_second().to_be_bytes());
            out.extend_from_slice(&datetime.nanosecond().to_be_bytes());
            match zone {
                Some(name) => {
                    put_varint(out, name.len());
                    out.extend_from_slice(name.as_bytes());
                }
                None => out.extend_from_slice(&datetime.offset_seconds().to_be_bytes()),
            }
        }
        Temporal::Duration(duration) => {
            out.push(TAG_DURATION);
            out.extend_from_slice(&duration.months().to_be_bytes());
            out.extend_from_slice(&duration.days().to_be_bytes());
            out.extend_from_slice(&duration.seconds().to_be_bytes());
            out.extend_from_slice(&duration.nanoseconds().to_be_bytes());
        }
    }
}

fn zigzag(value: i64) -> u64 {
    ((value << 1) ^ (value >> 63)) as u64
}

fn unzigzag(value: u64) -> i64 {
    (value >> 1) as i64 ^ -((value & 1) as i64)
}

fn fixed_width(tag: u8) -> Option<usize> {
    match tag {
        TAG_BOOLEAN => Some(1),
        TAG_INTEGER | TAG_FLOAT | TAG_DATE | TAG_LOCAL_TIME => Some(8),
        TAG_TIME => Some(12),
        TAG_LOCAL_DATETIME | TAG_OFFSET_DATETIME => Some(16),
        TAG_DURATION => Some(28),
        _ => None,
    }
}

pub(crate) fn put_varint(out: &mut Vec<u8>, value: usize) {
    put_varint_u64(out, value as u64);
}

pub(crate) fn put_varint_u64(out: &mut Vec<u8>, value: u64) {
    let mut rest = value;
    while rest >= 0x80 {
        out.push((rest & 0x7f) as u8 | 0x80);
        rest >>= 7;
    }
    out.push(rest as u8);
}

/// Reads one stored property value from the start of `bytes` both by decoding it and by
/// skipping it. Returns the decoded value encoded again with how many bytes decoding took, and
/// how many bytes skipping took. Only the fuzz targets call it.
#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub fn read_value_for_fuzzing(bytes: &[u8]) -> (Option<(Vec<u8>, usize)>, Option<usize>) {
    let mut decoding = Reader::new(bytes);
    let decoded = decoding.value().ok().map(|value| {
        let mut encoded = Vec::new();
        value.encode_into(&mut encoded);
        (encoded, bytes.len() - decoding.bytes.len())
    });
    let mut skipping = Reader::new(bytes);
    let skipped = skipping
        .skip_value()
        .ok()
        .map(|()| bytes.len() - skipping.bytes.len());
    (decoded, skipped)
}

#[derive(Clone, Debug)]
pub(crate) struct Reader<'a> {
    bytes: &'a [u8],
}

impl<'a> Reader<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }

    /// The bytes not read yet.
    pub(crate) fn remaining(&self) -> usize {
        self.bytes.len()
    }

    pub(crate) fn finish(&self) -> Result<(), DecodeError> {
        if self.bytes.is_empty() {
            Ok(())
        } else {
            Err(DecodeError::TrailingBytes)
        }
    }

    pub(crate) fn take(&mut self, count: usize) -> Result<&'a [u8], DecodeError> {
        let (head, tail) = self
            .bytes
            .split_at_checked(count)
            .ok_or(DecodeError::UnexpectedEnd)?;
        self.bytes = tail;
        Ok(head)
    }

    pub(crate) fn u8(&mut self) -> Result<u8, DecodeError> {
        let bytes = self.take(1)?;
        Ok(bytes[0])
    }

    pub(crate) fn u32(&mut self) -> Result<u32, DecodeError> {
        let mut array = [0u8; 4];
        array.copy_from_slice(self.take(4)?);
        Ok(u32::from_be_bytes(array))
    }

    pub(crate) fn u64(&mut self) -> Result<u64, DecodeError> {
        let mut array = [0u8; 8];
        array.copy_from_slice(self.take(8)?);
        Ok(u64::from_be_bytes(array))
    }

    pub(crate) fn varint(&mut self) -> Result<usize, DecodeError> {
        usize::try_from(self.varint_u64()?).map_err(|_| DecodeError::LengthOverflow)
    }

    pub(crate) fn varint_u64(&mut self) -> Result<u64, DecodeError> {
        let mut result: u64 = 0;
        for shift in (0..64).step_by(7) {
            let byte = self.u8()?;
            let bits = u64::from(byte & 0x7f);
            let shifted = bits << shift;
            if shifted >> shift != bits {
                return Err(DecodeError::LengthOverflow);
            }
            result |= shifted;
            if byte & 0x80 == 0 {
                return Ok(result);
            }
        }
        Err(DecodeError::LengthOverflow)
    }

    pub(crate) fn value(&mut self) -> Result<PropertyValue, DecodeError> {
        self.value_at_depth(0)
    }

    fn value_at_depth(&mut self, depth: usize) -> Result<PropertyValue, DecodeError> {
        match self.u8()? {
            TAG_BOOLEAN => match self.u8()? {
                0 => Ok(PropertyValue::Boolean(false)),
                1 => Ok(PropertyValue::Boolean(true)),
                other => Err(DecodeError::InvalidBoolean(other)),
            },
            TAG_INTEGER => Ok(PropertyValue::Integer(self.u64()? as i64)),
            TAG_VARINT_INTEGER => Ok(PropertyValue::Integer(unzigzag(self.varint_u64()?))),
            TAG_FLOAT => Ok(PropertyValue::Float(f64::from_bits(self.u64()?))),
            TAG_STRING => {
                let length = self.varint()?;
                let bytes = self.take(length)?;
                let text = std::str::from_utf8(bytes).map_err(|_| DecodeError::InvalidUtf8)?;
                Ok(PropertyValue::String(text.to_owned()))
            }
            TAG_BYTES => {
                let length = self.varint()?;
                Ok(PropertyValue::Bytes(self.take(length)?.to_vec()))
            }
            TAG_LIST => {
                if depth >= MAX_DEPTH {
                    return Err(DecodeError::NestingTooDeep);
                }
                let count = self.varint()?;
                let mut items = Vec::with_capacity(count.min(self.bytes.len()));
                for _ in 0..count {
                    items.push(self.value_at_depth(depth + 1)?);
                }
                Ok(PropertyValue::List(items))
            }
            TAG_SINGLE_FLOATS => {
                if depth >= MAX_DEPTH {
                    return Err(DecodeError::NestingTooDeep);
                }
                let count = self.varint()?;
                let bytes = self.take(count.checked_mul(4).ok_or(DecodeError::LengthOverflow)?)?;
                Ok(PropertyValue::List(
                    bytes
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .map(|chunk| {
                            PropertyValue::Float(f64::from(f32::from_bits(u32::from_be_bytes(
                                *chunk,
                            ))))
                        })
                        .collect(),
                ))
            }
            tag @ TAG_DATE..=TAG_DURATION => self.temporal(tag).map(PropertyValue::Temporal),
            tag => Err(DecodeError::UnknownTag(tag)),
        }
    }

    /// Moves past one value without building it.
    pub(crate) fn skip_value(&mut self) -> Result<(), DecodeError> {
        self.skip_at_depth(0)
    }

    fn skip_at_depth(&mut self, depth: usize) -> Result<(), DecodeError> {
        let tag = self.u8()?;
        let width = match fixed_width(tag) {
            Some(width) => width,
            None => match tag {
                TAG_STRING | TAG_BYTES => self.varint()?,
                TAG_VARINT_INTEGER => {
                    self.varint_u64()?;
                    0
                }
                TAG_ZONED_DATETIME => {
                    self.take(12)?;
                    self.varint()?
                }
                TAG_LIST => {
                    if depth >= MAX_DEPTH {
                        return Err(DecodeError::NestingTooDeep);
                    }
                    let mut remaining = self.varint()?;
                    while remaining > 0 {
                        let Some((tag, width)) = self
                            .bytes
                            .first()
                            .and_then(|tag| Some((*tag, fixed_width(*tag)?)))
                        else {
                            self.skip_at_depth(depth + 1)?;
                            remaining -= 1;
                            continue;
                        };
                        let stride = 1 + width;
                        let mut run = 1;
                        while run < remaining && self.bytes.get(run * stride) == Some(&tag) {
                            run += 1;
                        }
                        self.take(run * stride)?;
                        remaining -= run;
                    }
                    0
                }
                TAG_SINGLE_FLOATS => {
                    if depth >= MAX_DEPTH {
                        return Err(DecodeError::NestingTooDeep);
                    }
                    self.varint()?
                        .checked_mul(4)
                        .ok_or(DecodeError::LengthOverflow)?
                }
                tag => return Err(DecodeError::UnknownTag(tag)),
            },
        };
        self.take(width)?;
        Ok(())
    }

    fn i64(&mut self) -> Result<i64, DecodeError> {
        Ok(self.u64()? as i64)
    }

    fn temporal(&mut self, tag: u8) -> Result<Temporal, DecodeError> {
        let value = match tag {
            TAG_DATE => Date::from_days(self.i64()?).map(Temporal::Date),
            TAG_LOCAL_TIME => LocalTime::from_nanos(self.i64()?).map(Temporal::LocalTime),
            TAG_TIME => {
                let time = LocalTime::from_nanos(self.i64()?);
                let offset = self.u32()? as i32;
                time.and_then(|time| Time::new(time, offset))
                    .map(Temporal::Time)
            }
            TAG_LOCAL_DATETIME => {
                let date = Date::from_days(self.i64()?);
                let time = LocalTime::from_nanos(self.i64()?);
                date.zip(time)
                    .map(|(date, time)| Temporal::LocalDateTime(LocalDateTime::new(date, time)))
            }
            TAG_OFFSET_DATETIME => {
                let second = self.i64()?;
                let nanosecond = self.u32()?;
                let offset = self.u32()? as i32;
                DateTime::from_stored(second, nanosecond, offset, None).map(Temporal::DateTime)
            }
            TAG_ZONED_DATETIME => {
                let second = self.i64()?;
                let nanosecond = self.u32()?;
                let length = self.varint()?;
                let name = std::str::from_utf8(self.take(length)?)
                    .map_err(|_| DecodeError::InvalidUtf8)?;
                DateTime::from_stored(second, nanosecond, 0, Some(name)).map(Temporal::DateTime)
            }
            _ => {
                let months = self.i64()?;
                let days = self.i64()?;
                let seconds = self.i64()?;
                let nanoseconds = self.u32()?;
                Duration::from_stored(months, days, seconds, nanoseconds).map(Temporal::Duration)
            }
        };
        value.ok_or(DecodeError::InvalidTemporal)
    }
}

#[cfg(test)]
#[path = "value.test.rs"]
pub(crate) mod tests;
