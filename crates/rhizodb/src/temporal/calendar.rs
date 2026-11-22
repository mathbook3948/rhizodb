/// The earliest year a temporal value can have.
pub(crate) const MIN_YEAR: i64 = -999_999_999;
/// The latest year a temporal value can have.
pub(crate) const MAX_YEAR: i64 = 999_999_999;
/// Nanoseconds in one second.
pub(crate) const NANOS_PER_SECOND: i64 = 1_000_000_000;
/// Seconds in one day without a time-zone transition.
pub(crate) const SECONDS_PER_DAY: i64 = 86_400;
/// Nanoseconds in one day without a time-zone transition.
pub(crate) const NANOS_PER_DAY: i64 = SECONDS_PER_DAY * NANOS_PER_SECOND;
/// The day number of the earliest supported date.
pub(crate) const MIN_DAYS: i64 = days_from_civil(MIN_YEAR, 1, 1);
/// The day number of the latest supported date.
pub(crate) const MAX_DAYS: i64 = days_from_civil(MAX_YEAR, 12, 31);

/// The number of days from 1970-01-01 to a date of the proleptic Gregorian calendar.
pub(crate) const fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let month = month as i64;
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let shifted_month = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day as i64 - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The year, month, and day of a day number counted from 1970-01-01.
pub(crate) fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month as u32, day as u32)
}

/// Whether a year of the proleptic Gregorian calendar has 366 days.
pub(crate) fn is_leap_year(year: i64) -> bool {
    year.rem_euclid(4) == 0 && (year.rem_euclid(100) != 0 || year.rem_euclid(400) == 0)
}

/// The number of days in a month of a year.
pub(crate) fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        2 if is_leap_year(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// The number of days in a year.
pub(crate) fn days_in_year(year: i64) -> u32 {
    if is_leap_year(year) { 366 } else { 365 }
}

/// The ISO day of the week of a day number, from 1 for Monday to 7 for Sunday.
pub(crate) fn day_of_week(days: i64) -> u32 {
    (days + 3).rem_euclid(7) as u32 + 1
}

/// The day number of the Monday that starts week 1 of an ISO week-based year.
pub(crate) fn week_year_start(week_year: i64) -> i64 {
    let fourth_of_january = days_from_civil(week_year, 1, 4);
    fourth_of_january - i64::from(day_of_week(fourth_of_january)) + 1
}

/// The number of ISO weeks in a week-based year: 52 or 53.
pub(crate) fn weeks_in_year(week_year: i64) -> u32 {
    ((week_year_start(week_year + 1) - week_year_start(week_year)) / 7) as u32
}

/// The ISO week-based year and week of a day number.
pub(crate) fn iso_week(days: i64) -> (i64, u32) {
    let (year, _, _) = civil_from_days(days);
    let week_year = if days >= week_year_start(year + 1) {
        year + 1
    } else if days < week_year_start(year) {
        year - 1
    } else {
        year
    };
    let week = (days - week_year_start(week_year)) / 7 + 1;
    (week_year, week as u32)
}

/// The quarter, from 1 to 4, of a month.
pub(crate) fn quarter_of_month(month: u32) -> u32 {
    (month - 1) / 3 + 1
}

/// The number of days in a quarter of a year.
pub(crate) fn days_in_quarter(year: i64, quarter: u32) -> u32 {
    let first = 3 * (quarter - 1) + 1;
    (first..first + 3)
        .map(|month| days_in_month(year, month))
        .sum()
}

#[cfg(test)]
#[path = "calendar.test.rs"]
mod tests;
