use super::super::parse::stamp;
use super::*;

fn local(text: &str) -> Temporal {
    Temporal::LocalDateTime(stamp(text).unwrap().local)
}

fn months(count: i64) -> Duration {
    Duration::new(count, 0, 0, 0).unwrap()
}

#[test]
fn adding_months_clamps_the_day_to_the_month() {
    let moved = plus(&local("2015-01-31T00:00"), &months(1)).unwrap();
    assert_eq!(moved.to_string(), "2015-02-28T00:00");
}

#[test]
fn times_wrap_around_midnight() {
    let time = Temporal::LocalTime(LocalTime::from_hms_nano(23, 0, 0, 0).unwrap());
    let moved = plus(&time, &Duration::new(0, 0, 7200, 0).unwrap()).unwrap();
    assert_eq!(moved.to_string(), "01:00");
}

#[test]
fn durations_scale_with_carried_fractions() {
    let duration = months(1);
    assert_eq!(
        times_float(&duration, 0.5).unwrap().to_string(),
        "P15DT5H14M33S"
    );
    assert_eq!(
        divided_by_integer(&duration, 2).unwrap().to_string(),
        "P15DT5H14M33S"
    );
    assert_eq!(times_integer(&duration, -3).unwrap().to_string(), "P-3M");
}

#[test]
fn between_measures_months_then_days_then_time() {
    let from = local("2015-07-21T21:40:32.142");
    let to = local("2016-07-21T21:45:22.142");
    assert_eq!(
        between(&from, &to, Span::Whole).unwrap().to_string(),
        "P1YT4M50S"
    );
    assert_eq!(
        between(&from, &to, Span::Days).unwrap().to_string(),
        "P366D"
    );
    assert_eq!(
        between(&to, &from, Span::Whole).unwrap().to_string(),
        "P-1YT-4M-50S"
    );
}

#[test]
fn the_seconds_between_instants_at_the_edge_of_the_date_range_are_measured() {
    let edge = LocalDateTime::new(
        Date::from_ymd(-999_999_999, 1, 1).unwrap(),
        LocalTime::MIDNIGHT,
    );
    let utc = Temporal::DateTime(DateTime::with_offset(edge, 0).unwrap());
    let kolkata = Temporal::DateTime(DateTime::in_zone(edge, "Asia/Kolkata").unwrap());
    assert_eq!(
        between(&utc, &kolkata, Span::Seconds).unwrap().to_string(),
        "PT-5H-53M-28S"
    );
}
