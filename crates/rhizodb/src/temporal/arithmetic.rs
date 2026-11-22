use super::Date;
use super::DateTime;
use super::Duration;
use super::LocalDateTime;
use super::LocalTime;
use super::Temporal;
use super::Time;
use super::Zone;
use super::amount::Amount;
use super::amount::carry;
use super::calendar::civil_from_days;
use super::calendar::days_in_month;

/// `value` moved by `duration`: by its months, then its days, then its seconds and nanoseconds,
/// leaving out the units `value` does not have. A date moves by the whole days of the seconds.
/// `None` when the result is out of range.
pub(crate) fn plus(value: &Temporal, duration: &Duration) -> Option<Temporal> {
    let nanos = duration.total_nanos();
    Some(match value {
        Temporal::Date(date) => {
            let date = plus_days(plus_months(*date, duration.months)?, duration.days)?;
            Temporal::Date(plus_days(date, duration.seconds / 86_400)?)
        }
        Temporal::LocalTime(time) => {
            Temporal::LocalTime(LocalTime::wrapping(i128::from(time.nanos()) + nanos))
        }
        Temporal::Time(time) => Temporal::Time(Time {
            time: LocalTime::wrapping(i128::from(time.time.nanos()) + nanos),
            offset: time.offset,
        }),
        Temporal::LocalDateTime(local) => {
            let date = plus_days(plus_months(local.date, duration.months)?, duration.days)?;
            let moved = LocalDateTime::new(date, local.time);
            Temporal::LocalDateTime(LocalDateTime::from_nanos(moved.nanos() + nanos)?)
        }
        Temporal::DateTime(datetime) => {
            let zone = datetime.zone();
            let local = datetime.local();
            let date = plus_days(plus_months(local.date, duration.months)?, duration.days)?;
            let moved = DateTime::from_local(
                LocalDateTime::new(date, local.time),
                &zone,
                Some(datetime.offset),
            )?;
            Temporal::DateTime(from_epoch_nanos(moved.epoch_nanos() + nanos, &zone)?)
        }
        Temporal::Duration(other) => Temporal::Duration(sum(other, duration)?),
    })
}

fn from_epoch_nanos(nanos: i128, zone: &Zone) -> Option<DateTime> {
    let per_second = 1_000_000_000;
    let seconds = i64::try_from(nanos.div_euclid(per_second)).ok()?;
    DateTime::from_instant(seconds, nanos.rem_euclid(per_second) as i64, zone)
}

/// The sum of two durations, unit by unit.
pub(crate) fn sum(a: &Duration, b: &Duration) -> Option<Duration> {
    Duration::from_parts(
        i128::from(a.months) + i128::from(b.months),
        i128::from(a.days) + i128::from(b.days),
        0,
        a.total_nanos() + b.total_nanos(),
    )
}

/// The duration with every unit negated.
pub(crate) fn negated(duration: &Duration) -> Option<Duration> {
    Duration::from_parts(
        -i128::from(duration.months),
        -i128::from(duration.days),
        0,
        -duration.total_nanos(),
    )
}

/// A duration times a whole number, exactly.
pub(crate) fn times_integer(duration: &Duration, factor: i64) -> Option<Duration> {
    let factor = i128::from(factor);
    Duration::from_parts(
        i128::from(duration.months).checked_mul(factor)?,
        i128::from(duration.days).checked_mul(factor)?,
        0,
        duration.total_nanos().checked_mul(factor)?,
    )
}

/// A duration times a float, with the fractions of months, days, and seconds carried into the
/// smaller units.
pub(crate) fn times_float(duration: &Duration, factor: f64) -> Option<Duration> {
    let (months, days, seconds, nanoseconds) = carry(
        Amount::product(i128::from(duration.months), factor)?,
        Amount::product(i128::from(duration.days), factor)?,
        Amount::product(i128::from(duration.seconds), factor)?,
        Amount::product(i128::from(duration.nanoseconds), factor)?,
    )?;
    Duration::from_parts(months, days, seconds, nanoseconds)
}

/// A duration divided by a whole number other than zero, with the fractions of months, days,
/// and seconds carried into the smaller units.
pub(crate) fn divided_by_integer(duration: &Duration, divisor: i64) -> Option<Duration> {
    let divisor = i128::from(divisor);
    let (months, days, seconds, nanoseconds) = carry(
        Amount::quotient(i128::from(duration.months), divisor),
        Amount::quotient(i128::from(duration.days), divisor),
        Amount::quotient(i128::from(duration.seconds), divisor),
        Amount::quotient(i128::from(duration.nanoseconds), divisor),
    )?;
    Duration::from_parts(months, days, seconds, nanoseconds)
}

pub(crate) fn plus_months(date: Date, months: i64) -> Option<Date> {
    if months == 0 {
        return Some(date);
    }
    let (year, month, day) = civil_from_days(date.days);
    let index = i128::from(year) * 12 + i128::from(month - 1) + i128::from(months);
    let year = i64::try_from(index.div_euclid(12)).ok()?;
    let month = index.rem_euclid(12) as u32 + 1;
    Date::from_ymd(year, month, day.min(days_in_month(year, month)))
}

pub(crate) fn plus_days(date: Date, days: i64) -> Option<Date> {
    Date::from_days(date.days.checked_add(days)?)
}

/// Which units `duration.between` and its siblings measure in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Span {
    Whole,
    Months,
    Days,
    Seconds,
}

struct Point {
    date: Option<Date>,
    time: Option<LocalTime>,
    zone: Option<Zone>,
    offset: Option<i32>,
}

fn point(value: &Temporal) -> Option<Point> {
    Some(match value {
        Temporal::Date(date) => Point {
            date: Some(*date),
            time: None,
            zone: None,
            offset: None,
        },
        Temporal::LocalTime(time) => Point {
            date: None,
            time: Some(*time),
            zone: None,
            offset: None,
        },
        Temporal::Time(time) => Point {
            date: None,
            time: Some(time.time),
            zone: Some(Zone::Offset(time.offset)),
            offset: Some(time.offset),
        },
        Temporal::LocalDateTime(local) => Point {
            date: Some(local.date),
            time: Some(local.time),
            zone: None,
            offset: None,
        },
        Temporal::DateTime(datetime) => {
            let local = datetime.local();
            Point {
                date: Some(local.date),
                time: Some(local.time),
                zone: Some(datetime.zone()),
                offset: Some(datetime.offset),
            }
        }
        Temporal::Duration(_) => return None,
    })
}

/// The duration from `from` to `to` in the units of `span`. A value without a date or a zone
/// takes them from the other value. Without a date on both sides only the times of day are
/// measured. `None` when either value is a duration or the result is out of range.
pub(crate) fn between(from: &Temporal, to: &Temporal, span: Span) -> Option<Duration> {
    let (from, to) = (point(from)?, point(to)?);
    let dated = from.date.is_some() && to.date.is_some();
    let epoch = Date { days: 0 };
    let start = LocalDateTime::new(
        from.date.or(to.date).unwrap_or(epoch),
        from.time.unwrap_or(LocalTime::MIDNIGHT),
    );
    let end = LocalDateTime::new(
        to.date.or(from.date).unwrap_or(epoch),
        to.time.unwrap_or(LocalTime::MIDNIGHT),
    );
    let zones = match (from.zone, to.zone) {
        (Some(a), Some(b)) => Some((a, b)),
        (Some(a), None) => Some((a.clone(), a)),
        (None, Some(b)) => Some((b.clone(), b)),
        (None, None) => None,
    };
    let instants = match &zones {
        Some((from_zone, to_zone)) => Some((
            DateTime::from_local(start, from_zone, from.offset)?,
            DateTime::from_local(end, to_zone, to.offset)?,
        )),
        None => None,
    };
    let elapsed = |start: LocalDateTime| -> Option<i128> {
        Some(match (&zones, &instants) {
            (Some((zone, _)), Some((first, last))) => {
                last.epoch_nanos()
                    - DateTime::from_local(start, zone, Some(first.offset))?.epoch_nanos()
            }
            _ => end.nanos() - start.nanos(),
        })
    };
    if !dated {
        return match span {
            Span::Months | Span::Days => Some(Duration::ZERO),
            Span::Whole | Span::Seconds => Duration::from_parts(0, 0, 0, elapsed(start)?),
        };
    }
    let end_seen = match (&zones, &instants) {
        (Some((zone, _)), Some((_, last))) => {
            DateTime::from_instant(last.epoch_second, last.nanosecond, zone)?.local()
        }
        _ => end,
    };
    match span {
        Span::Months => Duration::from_parts(i128::from(months_until(start, end_seen)), 0, 0, 0),
        Span::Days => Duration::from_parts(0, i128::from(days_until(start, end_seen)), 0, 0),
        Span::Seconds => Duration::from_parts(0, 0, 0, elapsed(start)?),
        Span::Whole => {
            let months = months_until(start, end_seen);
            let shifted = LocalDateTime::new(plus_months(start.date, months)?, start.time);
            let days = days_until(shifted, end_seen);
            let shifted = LocalDateTime::new(plus_days(shifted.date, days)?, start.time);
            Duration::from_parts(i128::from(months), i128::from(days), 0, elapsed(shifted)?)
        }
    }
}

fn end_date(start: LocalDateTime, end: LocalDateTime) -> i64 {
    let days = end.date.days;
    if days > start.date.days && end.time < start.time {
        days - 1
    } else if days < start.date.days && end.time > start.time {
        days + 1
    } else {
        days
    }
}

fn months_until(start: LocalDateTime, end: LocalDateTime) -> i64 {
    let packed = |days: i64| {
        let (year, month, day) = civil_from_days(days);
        (i128::from(year) * 12 + i128::from(month)) * 32 + i128::from(day)
    };
    ((packed(end_date(start, end)) - packed(start.date.days)) / 32) as i64
}

fn days_until(start: LocalDateTime, end: LocalDateTime) -> i64 {
    end_date(start, end) - start.date.days
}

#[cfg(test)]
#[path = "arithmetic.test.rs"]
mod tests;
