use super::*;

#[test]
fn values_print_in_iso_8601() {
    let date = Date::from_ymd(1984, 10, 11).unwrap();
    assert_eq!(date.to_string(), "1984-10-11");
    assert_eq!(Date::from_ymd(-1, 1, 1).unwrap().to_string(), "-0001-01-01");
    assert_eq!(
        Date::from_ymd(12_345, 1, 1).unwrap().to_string(),
        "+12345-01-01"
    );
    let time = LocalTime::from_hms_nano(12, 31, 14, 645_000_000).unwrap();
    assert_eq!(time.to_string(), "12:31:14.645");
    assert_eq!(
        Time::new(time, -9000).unwrap().to_string(),
        "12:31:14.645-02:30"
    );
    let local = LocalDateTime::new(date, LocalTime::MIDNIGHT);
    assert_eq!(
        DateTime::with_offset(local, 0).unwrap().to_string(),
        "1984-10-11T00:00Z"
    );
    assert_eq!(
        DateTime::in_zone(local, "europe/stockholm")
            .unwrap()
            .to_string(),
        "1984-10-11T00:00+01:00[Europe/Stockholm]"
    );
    assert_eq!(
        Duration::new(14, 3, -1, 0).unwrap().to_string(),
        "P1Y2M3DT-1S"
    );
    assert_eq!(Duration::ZERO.to_string(), "PT0S");
}

#[test]
fn constructors_reject_out_of_range_components() {
    assert_eq!(Date::from_ymd(2015, 2, 29), None);
    assert_eq!(LocalTime::from_hms_nano(24, 0, 0, 0), None);
    assert_eq!(Time::new(LocalTime::MIDNIGHT, 18 * 3600 + 1), None);
    let local = LocalDateTime::new(Date::from_ymd(2000, 1, 1).unwrap(), LocalTime::MIDNIGHT);
    assert_eq!(DateTime::in_zone(local, "Nowhere/City"), None);
}

#[test]
fn times_order_by_instant_and_durations_do_not_compare() {
    let noon = LocalTime::from_hms_nano(12, 0, 0, 0).unwrap();
    let early = Temporal::Time(Time::new(noon, 3600).unwrap());
    let late = Temporal::Time(Time::new(noon, 0).unwrap());
    assert_eq!(early.compare(&late), Some(Ordering::Less));
    let duration = Temporal::Duration(Duration::ZERO);
    assert_eq!(duration.compare(&duration), None);
    assert_eq!(early.order(&duration), Ordering::Less);
}

#[test]
fn a_skipped_local_time_moves_forward_by_the_gap() {
    let local = LocalDateTime::new(
        Date::from_ymd(2017, 3, 26).unwrap(),
        LocalTime::from_hms_nano(2, 30, 0, 0).unwrap(),
    );
    let datetime = DateTime::in_zone(local, "Europe/Stockholm").unwrap();
    assert_eq!(
        datetime.to_string(),
        "2017-03-26T03:30+02:00[Europe/Stockholm]"
    );
}
