mod amount;
pub(crate) mod arithmetic;
pub(crate) mod build;
pub(crate) mod calendar;
mod parse;
pub(crate) mod zone;

use std::cmp::Ordering;
use std::fmt;
use std::fmt::Write;

use self::calendar::MAX_DAYS;
use self::calendar::MIN_DAYS;
use self::calendar::NANOS_PER_DAY;
use self::calendar::NANOS_PER_SECOND;
use self::calendar::SECONDS_PER_DAY;
use self::calendar::civil_from_days;
use self::calendar::days_from_civil;
use self::calendar::days_in_month;
use self::zone::NamedZone;
use self::zone::is_valid_offset;

/// A date, time, or duration value of openCypher.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Temporal {
    /// A calendar date.
    Date(Date),
    /// A time of day without a UTC offset.
    LocalTime(LocalTime),
    /// A time of day with a UTC offset.
    Time(Time),
    /// A date and time of day without a time zone.
    LocalDateTime(LocalDateTime),
    /// An instant with the time zone it is seen in.
    DateTime(DateTime),
    /// An amount of time in months, days, seconds, and nanoseconds.
    Duration(Duration),
}

/// A date of the proleptic Gregorian calendar, in years -999,999,999 to 999,999,999.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Date {
    days: i64,
}

/// A time of day with nanosecond precision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LocalTime {
    nanos: i64,
}

/// A time of day with a UTC offset of at most 18 hours either way.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Time {
    time: LocalTime,
    offset: i32,
}

/// A date and a time of day, without a time zone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LocalDateTime {
    date: Date,
    time: LocalTime,
}

/// An instant together with the time zone it is seen in: a fixed UTC offset or an IANA zone.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DateTime {
    epoch_second: i64,
    nanosecond: i64,
    offset: i32,
    zone: Option<String>,
}

/// An amount of time in months, days, seconds, and nanoseconds, each counted on its own because
/// months and days have no fixed length.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Duration {
    months: i64,
    days: i64,
    seconds: i64,
    nanoseconds: i64,
}

impl Date {
    /// The date with this year, month, and day, or `None` when there is no such date in the
    /// supported range.
    pub fn from_ymd(year: i64, month: u32, day: u32) -> Option<Date> {
        if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
            return None;
        }
        if !(calendar::MIN_YEAR..=calendar::MAX_YEAR).contains(&year) {
            return None;
        }
        Date::from_days(days_from_civil(year, month, day))
    }

    /// The year.
    pub fn year(self) -> i64 {
        civil_from_days(self.days).0
    }

    /// The month, from 1 to 12.
    pub fn month(self) -> u32 {
        civil_from_days(self.days).1
    }

    /// The day of the month, from 1.
    pub fn day(self) -> u32 {
        civil_from_days(self.days).2
    }

    pub(crate) fn from_days(days: i64) -> Option<Date> {
        (MIN_DAYS..=MAX_DAYS)
            .contains(&days)
            .then_some(Date { days })
    }

    pub(crate) fn days(self) -> i64 {
        self.days
    }
}

impl LocalTime {
    /// Midnight.
    pub const MIDNIGHT: LocalTime = LocalTime { nanos: 0 };

    /// The time with this hour, minute, second, and nanosecond, or `None` when one is out of
    /// range.
    pub fn from_hms_nano(hour: u32, minute: u32, second: u32, nanosecond: u32) -> Option<Self> {
        if hour > 23 || minute > 59 || second > 59 || i64::from(nanosecond) >= NANOS_PER_SECOND {
            return None;
        }
        let seconds = i64::from(hour) * 3600 + i64::from(minute) * 60 + i64::from(second);
        LocalTime::from_nanos(seconds * NANOS_PER_SECOND + i64::from(nanosecond))
    }

    /// The hour, from 0 to 23.
    pub fn hour(self) -> u32 {
        (self.nanos / (3600 * NANOS_PER_SECOND)) as u32
    }

    /// The minute of the hour.
    pub fn minute(self) -> u32 {
        (self.nanos / (60 * NANOS_PER_SECOND) % 60) as u32
    }

    /// The second of the minute.
    pub fn second(self) -> u32 {
        (self.nanos / NANOS_PER_SECOND % 60) as u32
    }

    /// The nanosecond of the second.
    pub fn nanosecond(self) -> u32 {
        (self.nanos % NANOS_PER_SECOND) as u32
    }

    pub(crate) fn from_nanos(nanos: i64) -> Option<LocalTime> {
        (0..NANOS_PER_DAY)
            .contains(&nanos)
            .then_some(LocalTime { nanos })
    }

    pub(crate) fn wrapping(nanos: i128) -> LocalTime {
        LocalTime {
            nanos: nanos.rem_euclid(i128::from(NANOS_PER_DAY)) as i64,
        }
    }

    pub(crate) fn nanos(self) -> i64 {
        self.nanos
    }
}

impl Time {
    /// The time of day `time` at a UTC offset in seconds, or `None` when the offset is more than
    /// 18 hours.
    pub fn new(time: LocalTime, offset_seconds: i32) -> Option<Time> {
        is_valid_offset(offset_seconds).then_some(Time {
            time,
            offset: offset_seconds,
        })
    }

    /// The time of day as the clocks at the offset show it.
    pub fn local_time(self) -> LocalTime {
        self.time
    }

    /// The UTC offset in seconds.
    pub fn offset_seconds(self) -> i32 {
        self.offset
    }

    fn utc_nanos(self) -> i64 {
        self.time.nanos - i64::from(self.offset) * NANOS_PER_SECOND
    }
}

impl LocalDateTime {
    /// The date `date` at the time of day `time`.
    pub fn new(date: Date, time: LocalTime) -> LocalDateTime {
        LocalDateTime { date, time }
    }

    /// The date.
    pub fn date(self) -> Date {
        self.date
    }

    /// The time of day.
    pub fn time(self) -> LocalTime {
        self.time
    }

    pub(crate) fn from_nanos(nanos: i128) -> Option<LocalDateTime> {
        let per_day = i128::from(NANOS_PER_DAY);
        let days = i64::try_from(nanos.div_euclid(per_day)).ok()?;
        Some(LocalDateTime {
            date: Date::from_days(days)?,
            time: LocalTime::wrapping(nanos),
        })
    }

    pub(crate) fn nanos(self) -> i128 {
        i128::from(self.date.days) * i128::from(NANOS_PER_DAY) + i128::from(self.time.nanos)
    }

    fn second(self) -> i64 {
        self.date.days * SECONDS_PER_DAY + self.time.nanos / NANOS_PER_SECOND
    }
}

/// The zone a datetime is seen in.
#[derive(Clone, Debug)]
pub(crate) enum Zone {
    Offset(i32),
    Named(NamedZone),
}

impl Zone {
    /// UTC as a fixed offset.
    pub(crate) const UTC: Zone = Zone::Offset(0);

    fn instant_of(&self, local_second: i64, preferred: Option<i32>) -> i64 {
        match self {
            Zone::Offset(offset) => local_second - i64::from(*offset),
            Zone::Named(zone) => zone.instant_of(local_second, preferred),
        }
    }

    fn offset_at(&self, epoch_second: i64) -> i32 {
        match self {
            Zone::Offset(offset) => *offset,
            Zone::Named(zone) => zone.offset_at(epoch_second),
        }
    }

    fn name(&self) -> Option<String> {
        match self {
            Zone::Offset(_) => None,
            Zone::Named(zone) => Some(zone.name.clone()),
        }
    }
}

impl DateTime {
    /// The local date and time `local` at a fixed UTC offset in seconds, or `None` when the
    /// offset is more than 18 hours or the instant is out of range.
    pub fn with_offset(local: LocalDateTime, offset_seconds: i32) -> Option<DateTime> {
        if !is_valid_offset(offset_seconds) {
            return None;
        }
        DateTime::from_local(local, &Zone::Offset(offset_seconds), None)
    }

    /// The local date and time `local` in the IANA zone `zone`, or `None` when the zone is
    /// unknown. A local time skipped by a transition moves forward by the length of the gap, and
    /// a local time that occurs twice takes the earlier offset.
    pub fn in_zone(local: LocalDateTime, zone: &str) -> Option<DateTime> {
        DateTime::from_local(local, &Zone::Named(NamedZone::find(zone)?), None)
    }

    /// Seconds from 1970-01-01T00:00Z to the instant, rounded down.
    pub fn epoch_second(&self) -> i64 {
        self.epoch_second
    }

    /// The nanosecond of the second of the instant.
    pub fn nanosecond(&self) -> u32 {
        self.nanosecond as u32
    }

    /// The UTC offset in effect at the instant, in seconds.
    pub fn offset_seconds(&self) -> i32 {
        self.offset
    }

    /// The IANA zone name, or `None` for a fixed offset.
    pub fn zone_name(&self) -> Option<&str> {
        self.zone.as_deref()
    }

    /// The date and time of day as the clocks of the zone show them.
    pub fn local(&self) -> LocalDateTime {
        let local_second = self.epoch_second + i64::from(self.offset);
        LocalDateTime {
            date: Date {
                days: local_second.div_euclid(SECONDS_PER_DAY),
            },
            time: LocalTime {
                nanos: local_second.rem_euclid(SECONDS_PER_DAY) * NANOS_PER_SECOND
                    + self.nanosecond,
            },
        }
    }

    pub(crate) fn from_local(
        local: LocalDateTime,
        zone: &Zone,
        preferred: Option<i32>,
    ) -> Option<DateTime> {
        let epoch_second = zone.instant_of(local.second(), preferred);
        DateTime::from_instant(epoch_second, local.time.nanos % NANOS_PER_SECOND, zone)
    }

    pub(crate) fn from_instant(epoch_second: i64, nanosecond: i64, zone: &Zone) -> Option<Self> {
        let epoch_second = epoch_second.checked_add(nanosecond.div_euclid(NANOS_PER_SECOND))?;
        let offset = zone.offset_at(epoch_second);
        let local_days = (epoch_second.checked_add(i64::from(offset))?).div_euclid(SECONDS_PER_DAY);
        Date::from_days(local_days)?;
        Some(DateTime {
            epoch_second,
            nanosecond: nanosecond.rem_euclid(NANOS_PER_SECOND),
            offset,
            zone: zone.name(),
        })
    }

    /// The datetime stored as an instant with a fixed offset, or with an IANA zone name whose
    /// offset at the instant is looked up again. `None` when the parts are out of range or the
    /// zone is unknown.
    pub(crate) fn from_stored(
        epoch_second: i64,
        nanosecond: u32,
        offset: i32,
        zone: Option<&str>,
    ) -> Option<DateTime> {
        if i64::from(nanosecond) >= NANOS_PER_SECOND {
            return None;
        }
        let zone = match zone {
            Some(name) => Zone::Named(NamedZone::find(name)?),
            None if is_valid_offset(offset) => Zone::Offset(offset),
            None => return None,
        };
        DateTime::from_instant(epoch_second, i64::from(nanosecond), &zone)
    }

    pub(crate) fn zone(&self) -> Zone {
        match self.zone.as_deref().and_then(NamedZone::find) {
            Some(zone) => Zone::Named(zone),
            None => Zone::Offset(self.offset),
        }
    }

    pub(crate) fn epoch_nanos(&self) -> i128 {
        i128::from(self.epoch_second) * i128::from(NANOS_PER_SECOND) + i128::from(self.nanosecond)
    }
}

/// The instant the system clock reads now, in UTC.
pub(crate) fn now() -> DateTime {
    let (epoch_second, nanosecond) = zone::system_now();
    DateTime::from_instant(epoch_second, nanosecond, &Zone::UTC).unwrap_or(DateTime {
        epoch_second: 0,
        nanosecond: 0,
        offset: 0,
        zone: None,
    })
}

impl Duration {
    /// The duration of `months` months, `days` days, and `seconds` seconds plus `nanoseconds`
    /// nanoseconds, or `None` when the seconds overflow.
    pub fn new(months: i64, days: i64, seconds: i64, nanoseconds: i64) -> Option<Duration> {
        Duration::from_parts(
            i128::from(months),
            i128::from(days),
            i128::from(seconds),
            i128::from(nanoseconds),
        )
    }

    /// The months.
    pub fn months(self) -> i64 {
        self.months
    }

    /// The days.
    pub fn days(self) -> i64 {
        self.days
    }

    /// The whole seconds, rounded down, so that the nanoseconds are never negative.
    pub fn seconds(self) -> i64 {
        self.seconds
    }

    /// The nanoseconds past the whole seconds, below one second.
    pub fn nanoseconds(self) -> u32 {
        self.nanoseconds as u32
    }

    pub(crate) const ZERO: Duration = Duration {
        months: 0,
        days: 0,
        seconds: 0,
        nanoseconds: 0,
    };

    pub(crate) fn from_parts(
        months: i128,
        days: i128,
        seconds: i128,
        nanoseconds: i128,
    ) -> Option<Duration> {
        let per_second = i128::from(NANOS_PER_SECOND);
        let seconds = seconds.checked_add(nanoseconds.div_euclid(per_second))?;
        Some(Duration {
            months: i64::try_from(months).ok()?,
            days: i64::try_from(days).ok()?,
            seconds: i64::try_from(seconds).ok()?,
            nanoseconds: nanoseconds.rem_euclid(per_second) as i64,
        })
    }

    /// The duration stored as its four parts, or `None` when the nanoseconds reach a second.
    pub(crate) fn from_stored(
        months: i64,
        days: i64,
        seconds: i64,
        nanoseconds: u32,
    ) -> Option<Duration> {
        (i64::from(nanoseconds) < NANOS_PER_SECOND).then_some(Duration {
            months,
            days,
            seconds,
            nanoseconds: i64::from(nanoseconds),
        })
    }

    pub(crate) fn total_nanos(self) -> i128 {
        i128::from(self.seconds) * i128::from(NANOS_PER_SECOND) + i128::from(self.nanoseconds)
    }
}

impl Temporal {
    fn rank(&self) -> u8 {
        match self {
            Temporal::DateTime(_) => 0,
            Temporal::LocalDateTime(_) => 1,
            Temporal::Date(_) => 2,
            Temporal::Time(_) => 3,
            Temporal::LocalTime(_) => 4,
            Temporal::Duration(_) => 5,
        }
    }

    /// The total order `ORDER BY` uses: by type, then by instant or value, then by zone.
    pub(crate) fn order(&self, other: &Temporal) -> Ordering {
        match (self, other) {
            (Temporal::Date(a), Temporal::Date(b)) => a.cmp(b),
            (Temporal::LocalTime(a), Temporal::LocalTime(b)) => a.cmp(b),
            (Temporal::LocalDateTime(a), Temporal::LocalDateTime(b)) => a.cmp(b),
            (Temporal::Time(a), Temporal::Time(b)) => a
                .utc_nanos()
                .cmp(&b.utc_nanos())
                .then(a.offset.cmp(&b.offset)),
            (Temporal::DateTime(a), Temporal::DateTime(b)) => a
                .epoch_nanos()
                .cmp(&b.epoch_nanos())
                .then(a.offset.cmp(&b.offset))
                .then_with(|| a.zone.cmp(&b.zone)),
            (Temporal::Duration(a), Temporal::Duration(b)) => (a.months, a.days, a.seconds)
                .cmp(&(b.months, b.days, b.seconds))
                .then(a.nanoseconds.cmp(&b.nanoseconds)),
            _ => self.rank().cmp(&other.rank()),
        }
    }

    /// How two values compare for `<` and its siblings: values of one type other than durations
    /// compare in their sort order, and anything else does not compare.
    pub(crate) fn compare(&self, other: &Temporal) -> Option<Ordering> {
        let comparable = self.rank() == other.rank() && !matches!(self, Temporal::Duration(_));
        comparable.then(|| self.order(other))
    }

    /// The name of the type as error messages give it.
    pub(crate) fn type_name(&self) -> &'static str {
        match self {
            Temporal::Date(_) => "date",
            Temporal::LocalTime(_) => "local time",
            Temporal::Time(_) => "time",
            Temporal::LocalDateTime(_) => "local datetime",
            Temporal::DateTime(_) => "datetime",
            Temporal::Duration(_) => "duration",
        }
    }
}

impl fmt::Display for Date {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (year, month, day) = civil_from_days(self.days);
        if year > 9999 {
            write!(formatter, "+{year}")?;
        } else if year.abs() < 1000 {
            let sign = if year < 0 { "-" } else { "" };
            write!(formatter, "{sign}{:04}", year.abs())?;
        } else {
            write!(formatter, "{year}")?;
        }
        write!(formatter, "-{month:02}-{day:02}")
    }
}

impl fmt::Display for LocalTime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:02}:{:02}", self.hour(), self.minute())?;
        let (second, nanosecond) = (self.second(), self.nanosecond());
        if second == 0 && nanosecond == 0 {
            return Ok(());
        }
        write!(formatter, ":{second:02}")?;
        if nanosecond == 0 {
            Ok(())
        } else if nanosecond % 1_000_000 == 0 {
            write!(formatter, ".{:03}", nanosecond / 1_000_000)
        } else if nanosecond % 1000 == 0 {
            write!(formatter, ".{:06}", nanosecond / 1000)
        } else {
            write!(formatter, ".{nanosecond:09}")
        }
    }
}

/// Writes a UTC offset as `Z` or `+HH:MM`, with `:SS` when the offset has seconds.
pub(crate) fn write_offset(formatter: &mut impl fmt::Write, offset: i32) -> fmt::Result {
    if offset == 0 {
        return formatter.write_char('Z');
    }
    let sign = if offset < 0 { '-' } else { '+' };
    let offset = offset.unsigned_abs();
    write!(
        formatter,
        "{sign}{:02}:{:02}",
        offset / 3600,
        offset / 60 % 60
    )?;
    if !offset.is_multiple_of(60) {
        write!(formatter, ":{:02}", offset % 60)?;
    }
    Ok(())
}

impl fmt::Display for Time {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.time)?;
        write_offset(formatter, self.offset)
    }
}

impl fmt::Display for LocalDateTime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}T{}", self.date, self.time)
    }
}

impl fmt::Display for DateTime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.local())?;
        write_offset(formatter, self.offset)?;
        match &self.zone {
            Some(zone) => write!(formatter, "[{zone}]"),
            None => Ok(()),
        }
    }
}

impl fmt::Display for Duration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let total = self.total_nanos();
        if self.months == 0 && self.days == 0 && total == 0 {
            return formatter.write_str("PT0S");
        }
        formatter.write_char('P')?;
        let (years, months) = (self.months / 12, self.months % 12);
        if years != 0 {
            write!(formatter, "{years}Y")?;
        }
        if months != 0 {
            write!(formatter, "{months}M")?;
        }
        if self.days != 0 {
            write!(formatter, "{}D", self.days)?;
        }
        if total == 0 {
            return Ok(());
        }
        formatter.write_char('T')?;
        let sign = if total < 0 { "-" } else { "" };
        let magnitude = total.unsigned_abs();
        let per_second = NANOS_PER_SECOND as u128;
        let (seconds, fraction) = (magnitude / per_second, magnitude % per_second);
        let (hours, minutes, seconds) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
        if hours != 0 {
            write!(formatter, "{sign}{hours}H")?;
        }
        if minutes != 0 {
            write!(formatter, "{sign}{minutes}M")?;
        }
        if seconds != 0 || fraction != 0 {
            write!(formatter, "{sign}{seconds}")?;
            if fraction != 0 {
                let digits = format!("{fraction:09}");
                write!(formatter, ".{}", digits.trim_end_matches('0'))?;
            }
            formatter.write_char('S')?;
        }
        Ok(())
    }
}

impl fmt::Display for Temporal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Temporal::Date(value) => value.fmt(formatter),
            Temporal::LocalTime(value) => value.fmt(formatter),
            Temporal::Time(value) => value.fmt(formatter),
            Temporal::LocalDateTime(value) => value.fmt(formatter),
            Temporal::DateTime(value) => value.fmt(formatter),
            Temporal::Duration(value) => value.fmt(formatter),
        }
    }
}

#[cfg(test)]
#[path = "temporal.test.rs"]
mod tests;
