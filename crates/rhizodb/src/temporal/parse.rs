use super::Date;
use super::Duration;
use super::LocalDateTime;
use super::LocalTime;
use super::amount::Amount;
use super::amount::carry;
use super::calendar::MAX_YEAR;
use super::calendar::days_from_civil;
use super::calendar::days_in_year;
use super::calendar::week_year_start;
use super::calendar::weeks_in_year;
use super::zone::is_valid_offset;

/// A date and time read from text, with the offset and zone name it names, if any.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Stamp {
    pub(crate) local: LocalDateTime,
    pub(crate) offset: Option<i32>,
    pub(crate) zone: Option<String>,
}

/// A time of day read from text, with the offset it names, if any.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Clock {
    pub(crate) time: LocalTime,
    pub(crate) offset: Option<i32>,
}

/// Reads an ISO 8601 calendar, week, or ordinal date, in the extended or basic format, with
/// the month, week, or day left out where ISO allows it.
pub(crate) fn date(text: &str) -> Option<Date> {
    let (year, rest) = year(text)?;
    let (extended, rest) = match rest.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, rest),
    };
    if rest.is_empty() {
        return (!extended).then(|| Date::from_ymd(year, 1, 1)).flatten();
    }
    if let Some(rest) = rest.strip_prefix(['W', 'w']) {
        let (week, rest) = digits(rest, 2)?;
        let day = match rest {
            "" => 1,
            _ => {
                let rest = if extended {
                    rest.strip_prefix('-')?
                } else {
                    rest
                };
                let (day, rest) = digits(rest, 1)?;
                rest.is_empty().then_some(day)?
            }
        };
        if week == 0 || week > i64::from(weeks_in_year(year)) || !(1..=7).contains(&day) {
            return None;
        }
        return Date::from_days(week_year_start(year) + (week - 1) * 7 + day - 1);
    }
    let length = rest.len();
    if !rest
        .bytes()
        .all(|byte| byte.is_ascii_digit() || byte == b'-')
    {
        return None;
    }
    match (extended, length) {
        (_, 2) => Date::from_ymd(year, rest.parse().ok()?, 1),
        (_, 3) => {
            let ordinal: u32 = rest.parse().ok()?;
            if ordinal == 0 || ordinal > days_in_year(year) {
                return None;
            }
            Date::from_days(days_from_civil(year, 1, 1) + i64::from(ordinal) - 1)
        }
        (false, 4) => Date::from_ymd(year, rest[..2].parse().ok()?, rest[2..].parse().ok()?),
        (true, 5) if rest.as_bytes()[2] == b'-' => {
            Date::from_ymd(year, rest[..2].parse().ok()?, rest[3..].parse().ok()?)
        }
        _ => None,
    }
}

fn year(text: &str) -> Option<(i64, &str)> {
    match text.as_bytes().first()? {
        sign @ (b'+' | b'-') => {
            let rest = &text[1..];
            let end = rest
                .find(|character: char| !character.is_ascii_digit())
                .unwrap_or(rest.len());
            if !(4..=9).contains(&end) {
                return None;
            }
            let magnitude: i64 = rest[..end].parse().ok()?;
            let year = if *sign == b'-' { -magnitude } else { magnitude };
            (year.abs() <= MAX_YEAR).then_some((year, &rest[end..]))
        }
        _ => digits(text, 4),
    }
}

fn digits(text: &str, count: usize) -> Option<(i64, &str)> {
    let head = text.get(..count)?;
    if !head.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some((head.parse().ok()?, &text[count..]))
}

/// Reads an ISO 8601 time of day without an offset: hours, then optionally minutes, seconds,
/// and a fraction of a second, in the extended or basic format.
pub(crate) fn local_time(text: &str) -> Option<LocalTime> {
    let (hour, rest) = digits(text, 2)?;
    let mut parts = [hour, 0, 0];
    let mut rest = rest;
    let extended = rest.starts_with(':');
    let mut nanosecond = 0;
    for part in parts.iter_mut().skip(1) {
        if rest.is_empty() {
            break;
        }
        if extended {
            rest = rest.strip_prefix(':')?;
        }
        let (value, after) = digits(rest, 2)?;
        *part = value;
        rest = after;
    }
    if !rest.is_empty() {
        if text.len() - rest.len() < if extended { 8 } else { 6 } {
            return None;
        }
        let fraction = rest.strip_prefix(['.', ','])?;
        nanosecond = fraction_nanos(fraction)?;
    }
    LocalTime::from_hms_nano(
        u32::try_from(parts[0]).ok()?,
        u32::try_from(parts[1]).ok()?,
        u32::try_from(parts[2]).ok()?,
        nanosecond,
    )
}

fn fraction_nanos(fraction: &str) -> Option<u32> {
    if fraction.is_empty()
        || fraction.len() > 9
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let value: u32 = fraction.parse().ok()?;
    Some(value * 10u32.pow(9 - fraction.len() as u32))
}

/// Reads a UTC offset: `Z`, or a sign and hours with optional minutes and seconds, in the
/// extended or basic format.
pub(crate) fn offset(text: &str) -> Option<i32> {
    if text.eq_ignore_ascii_case("z") {
        return Some(0);
    }
    let sign = match text.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let rest = &text[1..];
    let (hours, rest) = digits(rest, 2)?;
    let extended = rest.starts_with(':');
    let mut total = hours * 3600;
    let mut rest = rest;
    for scale in [60, 1] {
        if rest.is_empty() {
            break;
        }
        if extended {
            rest = rest.strip_prefix(':')?;
        }
        let (value, after) = digits(rest, 2)?;
        if value > 59 {
            return None;
        }
        total += value * scale;
        rest = after;
    }
    if !rest.is_empty() {
        return None;
    }
    let total = i32::try_from(sign * total).ok()?;
    is_valid_offset(total).then_some(total)
}

/// Reads a time of day with an optional UTC offset.
pub(crate) fn time(text: &str) -> Option<Clock> {
    match text.find(['+', '-', 'Z', 'z']) {
        Some(split) => Some(Clock {
            time: local_time(&text[..split])?,
            offset: Some(offset(&text[split..])?),
        }),
        None => Some(Clock {
            time: local_time(text)?,
            offset: None,
        }),
    }
}

/// Reads a date, optionally followed by `T`, a time of day, a UTC offset, and an IANA zone
/// name in brackets.
pub(crate) fn stamp(text: &str) -> Option<Stamp> {
    let (text, zone) = match text.strip_suffix(']') {
        Some(text) => {
            let open = text.rfind('[')?;
            (&text[..open], Some(text[open + 1..].to_owned()))
        }
        None => (text, None),
    };
    let (date_text, clock) = match text.find(['T', 't']) {
        Some(split) => (&text[..split], time(&text[split + 1..])?),
        None => (
            text,
            Clock {
                time: LocalTime::MIDNIGHT,
                offset: None,
            },
        ),
    };
    Some(Stamp {
        local: LocalDateTime::new(date(date_text)?, clock.time),
        offset: clock.offset,
        zone,
    })
}

/// Reads an ISO 8601 duration: `P` followed by signed, possibly fractional amounts of years,
/// months, weeks, and days, then `T` and amounts of hours, minutes, and seconds, or the
/// alternative form `PYYYY-MM-DDThh:mm:ss`.
pub(crate) fn duration(text: &str) -> Option<Duration> {
    let rest = text.strip_prefix(['P', 'p'])?;
    if rest.is_empty() {
        return None;
    }
    let bytes = rest.as_bytes();
    if bytes.len() > 4 && bytes[..4].iter().all(u8::is_ascii_digit) && bytes[4] == b'-' {
        return alternative_duration(rest);
    }
    let (date_part, time_part) = match rest.find(['T', 't']) {
        Some(split) => (&rest[..split], Some(&rest[split + 1..])),
        None => (rest, None),
    };
    if time_part.is_some_and(str::is_empty) {
        return None;
    }
    let mut totals = [Amount::ZERO; 4];
    let mut any = false;
    for (part, units) in [
        (
            Some(date_part),
            &[('Y', 0, 12), ('M', 0, 1), ('W', 1, 7), ('D', 1, 1)][..],
        ),
        (time_part, &[('H', 2, 3600), ('M', 2, 60), ('S', 2, 1)][..]),
    ] {
        let Some(mut part) = part else {
            continue;
        };
        let mut next = 0;
        while !part.is_empty() {
            let end = part.find(|character: char| character.is_ascii_alphabetic())?;
            let unit = part[end..].chars().next()?.to_ascii_uppercase();
            let position = units[next..]
                .iter()
                .position(|(name, _, _)| *name == unit)?;
            let (_, slot, scale) = units[next + position];
            next += position + 1;
            totals[slot] = totals[slot].plus(Amount::of_text(scale, &part[..end])?)?;
            any = true;
            part = &part[end + 1..];
        }
    }
    if !any {
        return None;
    }
    let (months, days, seconds, nanoseconds) = carry(totals[0], totals[1], totals[2], totals[3])?;
    Duration::from_parts(months, days, seconds, nanoseconds)
}

fn alternative_duration(text: &str) -> Option<Duration> {
    let (date_text, time_text) = match text.find(['T', 't']) {
        Some(split) => (&text[..split], Some(&text[split + 1..])),
        None => (text, None),
    };
    let (years, rest) = digits(date_text, 4)?;
    let rest = rest.strip_prefix('-')?;
    let (months, rest) = digits(rest, 2)?;
    let rest = rest.strip_prefix('-')?;
    let (days, rest) = digits(rest, 2)?;
    if !rest.is_empty() {
        return None;
    }
    let (seconds, nanoseconds) = match time_text {
        Some(time_text) => {
            let time = local_time(time_text)?;
            let seconds = i64::from(time.hour()) * 3600
                + i64::from(time.minute()) * 60
                + i64::from(time.second());
            (seconds, i64::from(time.nanosecond()))
        }
        None => (0, 0),
    };
    Duration::from_parts(
        i128::from(years) * 12 + i128::from(months),
        i128::from(days),
        i128::from(seconds),
        i128::from(nanoseconds),
    )
}

#[cfg(test)]
#[path = "parse.test.rs"]
mod tests;
