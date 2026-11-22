use std::fmt;

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
use super::calendar::NANOS_PER_SECOND;
use super::calendar::civil_from_days;
use super::calendar::day_of_week;
use super::calendar::days_from_civil;
use super::calendar::days_in_month;
use super::calendar::days_in_quarter;
use super::calendar::days_in_year;
use super::calendar::iso_week;
use super::calendar::quarter_of_month;
use super::calendar::week_year_start;
use super::calendar::weeks_in_year;
use super::parse;
use super::write_offset;
use super::zone::NamedZone;

/// A value given for one key of a map that builds a temporal value.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Input {
    Integer(i64),
    Float(f64),
    Text(String),
    Temporal(Temporal),
}

/// The type of instant value to build.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Kind {
    Date,
    LocalTime,
    Time,
    LocalDateTime,
    DateTime,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Date => "date",
            Kind::LocalTime => "local time",
            Kind::Time => "time",
            Kind::LocalDateTime => "local datetime",
            Kind::DateTime => "datetime",
        }
    }

    fn has_date(self) -> bool {
        matches!(self, Kind::Date | Kind::LocalDateTime | Kind::DateTime)
    }

    fn has_time(self) -> bool {
        !matches!(self, Kind::Date)
    }

    fn has_zone(self) -> bool {
        matches!(self, Kind::Time | Kind::DateTime)
    }
}

/// Why a temporal value cannot be built or read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TemporalError {
    message: String,
}

impl fmt::Display for TemporalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

fn invalid(message: impl Into<String>) -> TemporalError {
    TemporalError {
        message: message.into(),
    }
}

fn out_of_range() -> TemporalError {
    invalid("the temporal value is out of the supported range")
}

/// A component of a temporal value read as a property.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Component {
    Integer(i64),
    Text(String),
}

/// The zone named by a `timezone` text: a UTC offset or an IANA zone name.
pub(crate) fn zone_named(text: &str) -> Result<Zone, TemporalError> {
    if let Some(offset) = parse::offset(text) {
        return Ok(Zone::Offset(offset));
    }
    NamedZone::find(text)
        .map(Zone::Named)
        .ok_or_else(|| invalid(format!("'{text}' is not a known time zone")))
}

fn narrow(kind: Kind, datetime: DateTime) -> Temporal {
    let local = datetime.local();
    match kind {
        Kind::Date => Temporal::Date(local.date),
        Kind::LocalTime => Temporal::LocalTime(local.time),
        Kind::Time => Temporal::Time(Time {
            time: local.time,
            offset: datetime.offset,
        }),
        Kind::LocalDateTime => Temporal::LocalDateTime(local),
        Kind::DateTime => Temporal::DateTime(datetime),
    }
}

/// The value of `kind` at the instant `now`, seen in the zone `zone` or in UTC.
pub(crate) fn current(
    kind: Kind,
    zone: Option<&str>,
    now: &DateTime,
) -> Result<Temporal, TemporalError> {
    let zone = match zone {
        Some(text) => zone_named(text)?,
        None => Zone::UTC,
    };
    let at =
        DateTime::from_instant(now.epoch_second, now.nanosecond, &zone).ok_or_else(out_of_range)?;
    Ok(narrow(kind, at))
}

/// The value of `kind` that `text` spells in ISO 8601.
pub(crate) fn parsed(kind: Kind, text: &str) -> Result<Temporal, TemporalError> {
    let value = match kind {
        Kind::Date => parse::date(text).map(Temporal::Date),
        Kind::LocalTime => parse::local_time(text).map(Temporal::LocalTime),
        Kind::Time => parse::time(text).map(|clock| {
            Temporal::Time(Time {
                time: clock.time,
                offset: clock.offset.unwrap_or(0),
            })
        }),
        Kind::LocalDateTime => parse::stamp(text)
            .filter(|stamp| stamp.offset.is_none() && stamp.zone.is_none())
            .map(|stamp| Temporal::LocalDateTime(stamp.local)),
        Kind::DateTime => match parse::stamp(text) {
            Some(stamp) => {
                let zone = match (&stamp.zone, stamp.offset) {
                    (Some(name), _) => zone_named(name)?,
                    (None, Some(offset)) => Zone::Offset(offset),
                    (None, None) => Zone::UTC,
                };
                Some(Temporal::DateTime(
                    DateTime::from_local(stamp.local, &zone, stamp.offset)
                        .ok_or_else(out_of_range)?,
                ))
            }
            None => None,
        },
    };
    value.ok_or_else(|| invalid(format!("'{text}' is not a valid {}", kind.name())))
}

/// The duration that `text` spells in ISO 8601.
pub(crate) fn parsed_duration(text: &str) -> Result<Duration, TemporalError> {
    parse::duration(text).ok_or_else(|| invalid(format!("'{text}' is not a valid duration")))
}

#[derive(Clone, Debug, Default)]
struct Parts {
    date: Option<Temporal>,
    time: Option<Temporal>,
    datetime: Option<Temporal>,
    timezone: Option<String>,
    year: Option<i64>,
    month: Option<i64>,
    day: Option<i64>,
    week: Option<i64>,
    day_of_week: Option<i64>,
    ordinal_day: Option<i64>,
    quarter: Option<i64>,
    day_of_quarter: Option<i64>,
    hour: Option<i64>,
    minute: Option<i64>,
    second: Option<i64>,
    millisecond: Option<i64>,
    microsecond: Option<i64>,
    nanosecond: Option<i64>,
}

impl Parts {
    fn read(fields: &[(String, Input)]) -> Result<Parts, TemporalError> {
        let mut parts = Parts::default();
        for (key, value) in fields {
            let lower = key.to_ascii_lowercase();
            let slot = match lower.as_str() {
                "date" | "time" | "datetime" => {
                    let Input::Temporal(base) = value else {
                        return Err(invalid(format!("'{key}' must be a temporal value")));
                    };
                    let slot = match lower.as_str() {
                        "date" => &mut parts.date,
                        "time" => &mut parts.time,
                        _ => &mut parts.datetime,
                    };
                    *slot = Some(base.clone());
                    continue;
                }
                "timezone" => {
                    let Input::Text(text) = value else {
                        return Err(invalid("'timezone' must be a string"));
                    };
                    parts.timezone = Some(text.clone());
                    continue;
                }
                "year" => &mut parts.year,
                "month" => &mut parts.month,
                "day" => &mut parts.day,
                "week" => &mut parts.week,
                "dayofweek" | "weekday" => &mut parts.day_of_week,
                "ordinalday" => &mut parts.ordinal_day,
                "quarter" => &mut parts.quarter,
                "dayofquarter" => &mut parts.day_of_quarter,
                "hour" => &mut parts.hour,
                "minute" => &mut parts.minute,
                "second" => &mut parts.second,
                "millisecond" => &mut parts.millisecond,
                "microsecond" => &mut parts.microsecond,
                "nanosecond" => &mut parts.nanosecond,
                _ => return Err(invalid(format!("'{key}' is not a temporal component"))),
            };
            let Input::Integer(number) = value else {
                return Err(invalid(format!("'{key}' must be an integer")));
            };
            *slot = Some(*number);
        }
        Ok(parts)
    }

    fn has_date_fields(&self) -> bool {
        [
            self.year,
            self.month,
            self.day,
            self.week,
            self.day_of_week,
            self.ordinal_day,
            self.quarter,
            self.day_of_quarter,
        ]
        .iter()
        .any(Option::is_some)
    }

    fn has_time_fields(&self) -> bool {
        [
            self.hour,
            self.minute,
            self.second,
            self.millisecond,
            self.microsecond,
            self.nanosecond,
        ]
        .iter()
        .any(Option::is_some)
    }

    fn has_subsecond(&self) -> bool {
        self.millisecond.is_some() || self.microsecond.is_some() || self.nanosecond.is_some()
    }
}

#[derive(Clone, Debug, Default)]
struct Frame {
    date: Option<Date>,
    time: Option<LocalTime>,
    zone: Option<(Zone, i32)>,
    reference: Option<Date>,
}

fn date_of(value: &Temporal) -> Option<Date> {
    match value {
        Temporal::Date(date) => Some(*date),
        Temporal::LocalDateTime(local) => Some(local.date),
        Temporal::DateTime(datetime) => Some(datetime.local().date),
        _ => None,
    }
}

fn time_of(value: &Temporal) -> Option<LocalTime> {
    match value {
        Temporal::LocalTime(time) => Some(*time),
        Temporal::Time(time) => Some(time.time),
        Temporal::LocalDateTime(local) => Some(local.time),
        Temporal::DateTime(datetime) => Some(datetime.local().time),
        _ => None,
    }
}

fn zone_of(value: &Temporal) -> Option<(Zone, i32)> {
    match value {
        Temporal::Time(time) => Some((Zone::Offset(time.offset), time.offset)),
        Temporal::DateTime(datetime) => Some((datetime.zone(), datetime.offset)),
        _ => None,
    }
}

fn checked(value: i64, low: i64, high: i64, name: &str) -> Result<i64, TemporalError> {
    if (low..=high).contains(&value) {
        Ok(value)
    } else {
        Err(invalid(format!(
            "{name} must be between {low} and {high}, not {value}"
        )))
    }
}

fn small(value: i64) -> u32 {
    u32::try_from(value).unwrap_or(0)
}

fn resolve_date(parts: &Parts, base: Option<Date>) -> Result<Option<Date>, TemporalError> {
    let week = parts.week.is_some() || parts.day_of_week.is_some();
    let ordinal = parts.ordinal_day.is_some();
    let quarter = parts.quarter.is_some() || parts.day_of_quarter.is_some();
    let calendar = parts.month.is_some() || parts.day.is_some();
    if [week, ordinal, quarter, calendar]
        .iter()
        .filter(|mode| **mode)
        .count()
        > 1
    {
        return Err(invalid(
            "the date components mix more than one kind of date",
        ));
    }
    let Some(base_days) = base.map(Date::days) else {
        let Some(year) = parts.year else {
            if parts.has_date_fields() {
                return Err(invalid("a date needs a year"));
            }
            return Ok(None);
        };
        return explicit_date(parts, year).map(Some);
    };
    let (base_year, base_month, base_day) = civil_from_days(base_days);
    let days = if week {
        let (base_week_year, base_week) = iso_week(base_days);
        let week_year = checked(
            parts.year.unwrap_or(base_week_year),
            -999_999_999,
            999_999_999,
            "year",
        )?;
        let weeks = i64::from(weeks_in_year(week_year));
        let week = match parts.week {
            Some(week) => checked(week, 1, weeks, "week")?,
            None => i64::from(base_week).min(weeks),
        };
        let day = match parts.day_of_week {
            Some(day) => checked(day, 1, 7, "dayOfWeek")?,
            None => i64::from(day_of_week(base_days)),
        };
        week_year_start(week_year) + (week - 1) * 7 + day - 1
    } else if ordinal {
        let year = checked(
            parts.year.unwrap_or(base_year),
            -999_999_999,
            999_999_999,
            "year",
        )?;
        let ordinal = parts.ordinal_day.unwrap_or(1);
        let ordinal = checked(ordinal, 1, i64::from(days_in_year(year)), "ordinalDay")?;
        days_from_civil(year, 1, 1) + ordinal - 1
    } else if quarter {
        let year = checked(
            parts.year.unwrap_or(base_year),
            -999_999_999,
            999_999_999,
            "year",
        )?;
        let base_quarter = quarter_of_month(base_month);
        let quarter = small(checked(
            parts.quarter.unwrap_or(i64::from(base_quarter)),
            1,
            4,
            "quarter",
        )?);
        let length = i64::from(days_in_quarter(year, quarter));
        let first_month = 3 * (quarter - 1) + 1;
        let day = match parts.day_of_quarter {
            Some(day) => checked(day, 1, length, "dayOfQuarter")?,
            None => {
                let first = days_from_civil(base_year, 3 * (base_quarter - 1) + 1, 1);
                (base_days - first + 1).min(length)
            }
        };
        days_from_civil(year, first_month, 1) + day - 1
    } else {
        let year = checked(
            parts.year.unwrap_or(base_year),
            -999_999_999,
            999_999_999,
            "year",
        )?;
        let month = small(checked(
            parts.month.unwrap_or(i64::from(base_month)),
            1,
            12,
            "month",
        )?);
        let length = i64::from(days_in_month(year, month));
        let day = match parts.day {
            Some(day) => checked(day, 1, length, "day")?,
            None => i64::from(base_day).min(length),
        };
        days_from_civil(year, month, small(day))
    };
    Date::from_days(days).map(Some).ok_or_else(out_of_range)
}

fn explicit_date(parts: &Parts, year: i64) -> Result<Date, TemporalError> {
    let year = checked(year, -999_999_999, 999_999_999, "year")?;
    let days = if parts.week.is_some() || parts.day_of_week.is_some() {
        let Some(week) = parts.week else {
            return Err(invalid("dayOfWeek needs a week"));
        };
        let week = checked(week, 1, i64::from(weeks_in_year(year)), "week")?;
        let day = checked(parts.day_of_week.unwrap_or(1), 1, 7, "dayOfWeek")?;
        week_year_start(year) + (week - 1) * 7 + day - 1
    } else if let Some(ordinal) = parts.ordinal_day {
        let ordinal = checked(ordinal, 1, i64::from(days_in_year(year)), "ordinalDay")?;
        days_from_civil(year, 1, 1) + ordinal - 1
    } else if parts.quarter.is_some() || parts.day_of_quarter.is_some() {
        let Some(quarter) = parts.quarter else {
            return Err(invalid("dayOfQuarter needs a quarter"));
        };
        let quarter = small(checked(quarter, 1, 4, "quarter")?);
        let length = i64::from(days_in_quarter(year, quarter));
        let day = checked(parts.day_of_quarter.unwrap_or(1), 1, length, "dayOfQuarter")?;
        days_from_civil(year, 3 * (quarter - 1) + 1, 1) + day - 1
    } else {
        if parts.day.is_some() && parts.month.is_none() {
            return Err(invalid("day needs a month"));
        }
        let month = small(checked(parts.month.unwrap_or(1), 1, 12, "month")?);
        let length = i64::from(days_in_month(year, month));
        let day = checked(parts.day.unwrap_or(1), 1, length, "day")?;
        days_from_civil(year, month, small(day))
    };
    Date::from_days(days).ok_or_else(out_of_range)
}

fn resolve_time(
    parts: &Parts,
    base: Option<LocalTime>,
    add_subsecond: bool,
) -> Result<Option<LocalTime>, TemporalError> {
    if base.is_none() {
        if !parts.has_time_fields() {
            return Ok(None);
        }
        let missing = (parts.minute.is_some() && parts.hour.is_none())
            || (parts.second.is_some() && parts.minute.is_none())
            || (parts.has_subsecond() && parts.second.is_none());
        if missing {
            return Err(invalid("a time component needs the larger components"));
        }
    }
    let base = base.unwrap_or(LocalTime::MIDNIGHT);
    let hour = checked(parts.hour.unwrap_or(i64::from(base.hour())), 0, 23, "hour")?;
    let minute = checked(
        parts.minute.unwrap_or(i64::from(base.minute())),
        0,
        59,
        "minute",
    )?;
    let second = checked(
        parts.second.unwrap_or(i64::from(base.second())),
        0,
        59,
        "second",
    )?;
    let base_subsecond = i64::from(base.nanosecond());
    let subsecond = if parts.has_subsecond() {
        let given = checked(parts.millisecond.unwrap_or(0), 0, 999, "millisecond")? * 1_000_000
            + checked(parts.microsecond.unwrap_or(0), 0, 999_999, "microsecond")? * 1000
            + checked(parts.nanosecond.unwrap_or(0), 0, 999_999_999, "nanosecond")?;
        let total = if add_subsecond {
            base_subsecond + given
        } else {
            given
        };
        checked(
            total,
            0,
            NANOS_PER_SECOND - 1,
            "the fraction of a second in nanoseconds",
        )?
    } else {
        base_subsecond
    };
    let nanos = ((hour * 60 + minute) * 60 + second) * NANOS_PER_SECOND + subsecond;
    LocalTime::from_nanos(nanos)
        .map(Some)
        .ok_or_else(out_of_range)
}

fn zoned(
    local: LocalDateTime,
    base: Option<(Zone, i32)>,
    timezone: Option<&str>,
    convert: bool,
) -> Result<DateTime, TemporalError> {
    let target = timezone.map(zone_named).transpose()?;
    let datetime = match (base, target) {
        (Some((zone, offset)), Some(target)) if convert => {
            let at = DateTime::from_local(local, &zone, Some(offset)).ok_or_else(out_of_range)?;
            DateTime::from_instant(at.epoch_second, at.nanosecond, &target)
        }
        (_, Some(target)) => DateTime::from_local(local, &target, None),
        (Some((zone, offset)), None) => DateTime::from_local(local, &zone, Some(offset)),
        (None, None) => DateTime::from_local(local, &Zone::UTC, None),
    };
    datetime.ok_or_else(out_of_range)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Rule {
    Construct,
    Truncate,
}

fn assemble(
    kind: Kind,
    parts: &Parts,
    frame: Frame,
    rule: Rule,
    now: &DateTime,
) -> Result<Temporal, TemporalError> {
    if !kind.has_date() && parts.has_date_fields() {
        return Err(invalid(format!("a {} has no date components", kind.name())));
    }
    if !kind.has_time() && parts.has_time_fields() {
        return Err(invalid("a date has no time components"));
    }
    if !kind.has_zone() && parts.timezone.is_some() {
        return Err(invalid(format!("a {} has no time zone", kind.name())));
    }
    let date = if kind.has_date() {
        resolve_date(parts, frame.date)?
    } else {
        None
    };
    let time = resolve_time(parts, frame.time, rule == Rule::Truncate)?;
    let needs_date = || invalid(format!("a {} needs a date", kind.name()));
    let convert = rule == Rule::Construct;
    Ok(match kind {
        Kind::Date => Temporal::Date(date.ok_or_else(needs_date)?),
        Kind::LocalTime => {
            Temporal::LocalTime(time.ok_or_else(|| invalid("a time needs an hour"))?)
        }
        Kind::LocalDateTime => Temporal::LocalDateTime(LocalDateTime::new(
            date.ok_or_else(needs_date)?,
            time.unwrap_or(LocalTime::MIDNIGHT),
        )),
        Kind::DateTime => {
            let local = LocalDateTime::new(
                date.ok_or_else(needs_date)?,
                time.unwrap_or(LocalTime::MIDNIGHT),
            );
            Temporal::DateTime(zoned(
                local,
                frame.zone,
                parts.timezone.as_deref(),
                convert,
            )?)
        }
        Kind::Time => {
            let time = time.ok_or_else(|| invalid("a time needs an hour"))?;
            let reference = frame.reference.unwrap_or(now.local().date);
            let at = zoned(
                LocalDateTime::new(reference, time),
                frame.zone,
                parts.timezone.as_deref(),
                convert,
            )?;
            Temporal::Time(Time {
                time: at.local().time,
                offset: at.offset,
            })
        }
    })
}

/// The value of `kind` that a map of components describes. The keys `date`, `time`, and
/// `datetime` name values whose components the others replace; a map with only `timezone`
/// gives the value of `kind` at `now` in that zone.
pub(crate) fn from_fields(
    kind: Kind,
    fields: &[(String, Input)],
    now: &DateTime,
) -> Result<Temporal, TemporalError> {
    let parts = Parts::read(fields)?;
    if fields.len() == 1 && parts.timezone.is_some() {
        return current(kind, parts.timezone.as_deref(), now);
    }
    if fields.is_empty() {
        return Err(invalid(format!("a {} needs components", kind.name())));
    }
    let base_error = |key: &str| {
        invalid(format!(
            "'{key}' does not hold what a {} needs",
            kind.name()
        ))
    };
    let mut frame = Frame::default();
    if let Some(base) = &parts.datetime {
        if !matches!(
            base,
            Temporal::LocalDateTime(_) | Temporal::DateTime(_) | Temporal::Date(_)
        ) {
            return Err(base_error("datetime"));
        }
        frame.date = date_of(base);
        frame.time = time_of(base);
        frame.zone = zone_of(base);
        frame.reference = date_of(base);
    }
    if let Some(base) = &parts.date {
        frame.date = Some(date_of(base).ok_or_else(|| base_error("date"))?);
    }
    if let Some(base) = &parts.time {
        frame.time = Some(time_of(base).ok_or_else(|| base_error("time"))?);
        frame.zone = zone_of(base);
        frame.reference = date_of(base);
    }
    if !kind.has_date() && (parts.date.is_some() || parts.datetime.is_some()) {
        return Err(base_error("date"));
    }
    if !kind.has_time() && parts.time.is_some() {
        return Err(base_error("time"));
    }
    assemble(kind, &parts, frame, Rule::Construct, now)
}

/// The value of `kind` that `value` converts to: its date, its time, or both, with its zone
/// where `kind` has one.
pub(crate) fn converted(
    kind: Kind,
    value: &Temporal,
    now: &DateTime,
) -> Result<Temporal, TemporalError> {
    let key = match kind {
        Kind::Date => "date",
        Kind::LocalTime | Kind::Time => "time",
        Kind::LocalDateTime | Kind::DateTime => "datetime",
    };
    let fields = [(key.to_owned(), Input::Temporal(value.clone()))];
    from_fields(kind, &fields, now)
}

/// `value` truncated to the start of its `unit`, as a value of `kind`, with the components of
/// `fields` then set and the sub-second ones added to what is left.
pub(crate) fn truncated(
    kind: Kind,
    unit: &str,
    value: &Temporal,
    fields: &[(String, Input)],
    now: &DateTime,
) -> Result<Temporal, TemporalError> {
    let parts = Parts::read(fields)?;
    if parts.date.is_some() || parts.time.is_some() || parts.datetime.is_some() {
        return Err(invalid("truncate takes no base values"));
    }
    let unit = unit.to_ascii_lowercase();
    let date_unit = matches!(
        unit.as_str(),
        "millennium" | "century" | "decade" | "year" | "weekyear" | "quarter" | "month" | "week"
    );
    let time_unit = match unit.as_str() {
        "day" => Some(86_400 * NANOS_PER_SECOND),
        "hour" => Some(3600 * NANOS_PER_SECOND),
        "minute" => Some(60 * NANOS_PER_SECOND),
        "second" => Some(NANOS_PER_SECOND),
        "millisecond" => Some(1_000_000),
        "microsecond" => Some(1000),
        _ => None,
    };
    if !date_unit && time_unit.is_none() {
        return Err(invalid(format!("'{unit}' is not a unit to truncate to")));
    }
    if kind == Kind::Date && time_unit.is_some_and(|size| size < 86_400 * NANOS_PER_SECOND) {
        return Err(invalid(format!("a date cannot be truncated to the {unit}")));
    }
    if !kind.has_date() && date_unit {
        return Err(invalid(format!(
            "a {} cannot be truncated to the {unit}",
            kind.name()
        )));
    }
    if matches!(value, Temporal::Duration(_)) {
        return Err(invalid("a duration cannot be truncated"));
    }
    let date = date_of(value);
    if kind.has_date() && date.is_none() {
        return Err(invalid(format!("a {} needs a date", kind.name())));
    }
    let time = time_of(value).unwrap_or(LocalTime::MIDNIGHT);
    let date = date.map(|date| truncate_date(date, &unit));
    let time = match time_unit {
        Some(size) => LocalTime {
            nanos: time.nanos - time.nanos % size,
        },
        None => LocalTime::MIDNIGHT,
    };
    let frame = Frame {
        date: if kind.has_date() { date } else { None },
        time: Some(time),
        zone: zone_of(value),
        reference: date,
    };
    assemble(kind, &parts, frame, Rule::Truncate, now)
}

fn truncate_date(date: Date, unit: &str) -> Date {
    let (year, month, _) = civil_from_days(date.days);
    let days = match unit {
        "millennium" => days_from_civil(year.div_euclid(1000) * 1000, 1, 1),
        "century" => days_from_civil(year.div_euclid(100) * 100, 1, 1),
        "decade" => days_from_civil(year.div_euclid(10) * 10, 1, 1),
        "year" => days_from_civil(year, 1, 1),
        "weekyear" => week_year_start(iso_week(date.days).0),
        "quarter" => days_from_civil(year, 3 * (quarter_of_month(month) - 1) + 1, 1),
        "month" => days_from_civil(year, month, 1),
        "week" => date.days - i64::from(day_of_week(date.days)) + 1,
        _ => date.days,
    };
    Date::from_days(days).unwrap_or(date)
}

/// The datetime in UTC `seconds` and `nanoseconds` after 1970-01-01T00:00Z.
pub(crate) fn from_epoch(seconds: i64, nanoseconds: i64) -> Result<Temporal, TemporalError> {
    DateTime::from_instant(seconds, nanoseconds, &Zone::UTC)
        .map(Temporal::DateTime)
        .ok_or_else(out_of_range)
}

/// The datetime in UTC `millis` milliseconds after 1970-01-01T00:00Z.
pub(crate) fn from_epoch_millis(millis: i64) -> Result<Temporal, TemporalError> {
    from_epoch(millis.div_euclid(1000), millis.rem_euclid(1000) * 1_000_000)
}

/// The duration that a map of amounts describes, with fractions carried into smaller units.
pub(crate) fn duration_from_fields(fields: &[(String, Input)]) -> Result<Duration, TemporalError> {
    let mut totals = [Amount::ZERO; 4];
    for (key, value) in fields {
        let (slot, scale) = match key.to_ascii_lowercase().as_str() {
            "years" => (0, 12),
            "quarters" => (0, 3),
            "months" => (0, 1),
            "weeks" => (1, 7),
            "days" => (1, 1),
            "hours" => (2, 3600),
            "minutes" => (2, 60),
            "seconds" => (2, 1),
            "milliseconds" => (3, 1_000_000),
            "microseconds" => (3, 1000),
            "nanoseconds" => (3, 1),
            _ => return Err(invalid(format!("'{key}' is not a duration component"))),
        };
        let amount = match value {
            Input::Integer(number) => i128::from(*number).checked_mul(scale).map(Amount::whole),
            Input::Float(number) => Amount::product(scale, *number),
            _ => return Err(invalid(format!("'{key}' must be a number"))),
        };
        let amount = amount.ok_or_else(out_of_range)?;
        totals[slot] = totals[slot].plus(amount).ok_or_else(out_of_range)?;
    }
    let (months, days, seconds, nanoseconds) =
        carry(totals[0], totals[1], totals[2], totals[3]).ok_or_else(out_of_range)?;
    Duration::from_parts(months, days, seconds, nanoseconds).ok_or_else(out_of_range)
}

fn offset_text(offset: i32) -> String {
    let mut text = String::new();
    let _ = write_offset(&mut text, offset);
    text
}

/// The component `name` of `value`, or `None` when values of its type have no such component.
pub(crate) fn component(value: &Temporal, name: &str) -> Option<Component> {
    let name = name.to_ascii_lowercase();
    if let Temporal::Duration(duration) = value {
        return duration_component(duration, &name).map(Component::Integer);
    }
    if let Some(date) = date_of(value) {
        let days = date.days;
        let (year, month, day) = civil_from_days(days);
        let (week_year, week) = iso_week(days);
        let quarter = quarter_of_month(month);
        let found = match name.as_str() {
            "year" => Some(year),
            "quarter" => Some(i64::from(quarter)),
            "month" => Some(i64::from(month)),
            "week" => Some(i64::from(week)),
            "weekyear" => Some(week_year),
            "day" => Some(i64::from(day)),
            "ordinalday" => Some(days - days_from_civil(year, 1, 1) + 1),
            "weekday" | "dayofweek" => Some(i64::from(day_of_week(days))),
            "dayofquarter" => Some(days - days_from_civil(year, 3 * (quarter - 1) + 1, 1) + 1),
            _ => None,
        };
        if let Some(found) = found {
            return Some(Component::Integer(found));
        }
    }
    if let Some(time) = time_of(value) {
        let found = match name.as_str() {
            "hour" => Some(time.hour()),
            "minute" => Some(time.minute()),
            "second" => Some(time.second()),
            "millisecond" => Some(time.nanosecond() / 1_000_000),
            "microsecond" => Some(time.nanosecond() / 1000),
            "nanosecond" => Some(time.nanosecond()),
            _ => None,
        };
        if let Some(found) = found {
            return Some(Component::Integer(i64::from(found)));
        }
    }
    let (zone_name, offset) = match value {
        Temporal::Time(time) => (None, time.offset),
        Temporal::DateTime(datetime) => (datetime.zone.clone(), datetime.offset),
        _ => return None,
    };
    match name.as_str() {
        "timezone" => Some(Component::Text(
            zone_name.unwrap_or_else(|| offset_text(offset)),
        )),
        "offset" => Some(Component::Text(offset_text(offset))),
        "offsetminutes" => Some(Component::Integer(i64::from(offset / 60))),
        "offsetseconds" => Some(Component::Integer(i64::from(offset))),
        _ => match value {
            Temporal::DateTime(datetime) => match name.as_str() {
                "epochseconds" => Some(Component::Integer(datetime.epoch_second)),
                "epochmillis" => Some(Component::Integer(
                    datetime
                        .epoch_second
                        .saturating_mul(1000)
                        .saturating_add(datetime.nanosecond / 1_000_000),
                )),
                _ => None,
            },
            _ => None,
        },
    }
}

fn duration_component(duration: &Duration, name: &str) -> Option<i64> {
    let Duration {
        months,
        days,
        seconds,
        nanoseconds,
    } = *duration;
    Some(match name {
        "years" => months / 12,
        "quarters" => months / 3,
        "months" => months,
        "weeks" => days / 7,
        "days" => days,
        "hours" => seconds / 3600,
        "minutes" => seconds / 60,
        "seconds" => seconds,
        "milliseconds" => seconds
            .saturating_mul(1000)
            .saturating_add(nanoseconds / 1_000_000),
        "microseconds" => seconds
            .saturating_mul(1_000_000)
            .saturating_add(nanoseconds / 1000),
        "nanoseconds" => seconds
            .saturating_mul(NANOS_PER_SECOND)
            .saturating_add(nanoseconds),
        "quartersofyear" => months % 12 / 3,
        "monthsofquarter" => months % 3,
        "monthsofyear" => months % 12,
        "daysofweek" => days % 7,
        "minutesofhour" => seconds / 60 % 60,
        "secondsofminute" => seconds % 60,
        "millisecondsofsecond" => nanoseconds / 1_000_000,
        "microsecondsofsecond" => nanoseconds / 1000,
        "nanosecondsofsecond" => nanoseconds,
        _ => return None,
    })
}

#[cfg(test)]
#[path = "build.test.rs"]
mod tests;
