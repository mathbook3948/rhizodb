use super::*;

fn day(text: &str) -> String {
    date(text).map(|date| date.to_string()).unwrap_or_default()
}

#[test]
fn dates_read_in_every_iso_form() {
    for (text, expected) in [
        ("2015-07-21", "2015-07-21"),
        ("20150721", "2015-07-21"),
        ("2015-07", "2015-07-01"),
        ("201507", "2015-07-01"),
        ("2015-W30-2", "2015-07-21"),
        ("2015W302", "2015-07-21"),
        ("2015-W30", "2015-07-20"),
        ("2015W30", "2015-07-20"),
        ("2015-202", "2015-07-21"),
        ("2015202", "2015-07-21"),
        ("2015", "2015-01-01"),
        ("+999999999-12-31", "+999999999-12-31"),
        ("-999999999-01-01", "-999999999-01-01"),
    ] {
        assert_eq!(day(text), expected, "{text}");
    }
}

#[test]
fn malformed_dates_are_rejected() {
    for text in [
        "2015-13-01",
        "2015-02-30",
        "2015-W54",
        "201",
        "2015-0721",
        "x",
    ] {
        assert_eq!(date(text), None, "{text}");
    }
}

#[test]
fn times_and_offsets_read_in_both_formats() {
    let clock = time("214032.142-0130").unwrap();
    assert_eq!(clock.time.to_string(), "21:40:32.142");
    assert_eq!(clock.offset, Some(-5400));
    assert_eq!(
        local_time("21").map(|time| time.to_string()),
        Some("21:00".to_owned())
    );
    assert_eq!(offset("-00:00"), Some(0));
    assert_eq!(offset("+02:05:59"), Some(7559));
    assert_eq!(offset("+19:00"), None);
    assert_eq!(local_time("2140.5"), None);
}

#[test]
fn stamps_carry_offsets_and_zone_names() {
    let stamp = stamp("2015-07-21T21:40:32.142+0845[Australia/Eucla]").unwrap();
    assert_eq!(stamp.local.to_string(), "2015-07-21T21:40:32.142");
    assert_eq!(stamp.offset, Some(31_500));
    assert_eq!(stamp.zone.as_deref(), Some("Australia/Eucla"));
}

#[test]
fn durations_carry_fractions_into_smaller_units() {
    for (text, expected) in [
        ("P14DT16H12M", "P14DT16H12M"),
        ("P5M1.5D", "P5M1DT12H"),
        ("P0.75M", "P22DT19H51M49.5S"),
        ("PT0.75M", "PT45S"),
        ("P2.5W", "P17DT12H"),
        ("P12Y5M14DT16H12M70S", "P12Y5M14DT16H13M10S"),
        ("P2012-02-02T14:37:21.545", "P2012Y2M2DT14H37M21.545S"),
        ("PT-0.3S", "PT-0.3S"),
        ("P-1Y-8M", "P-1Y-8M"),
        ("PT-1M-0.001S", "PT-1M-0.001S"),
    ] {
        let parsed = duration(text).map(|value| value.to_string());
        assert_eq!(parsed.as_deref(), Some(expected), "{text}");
    }
    assert_eq!(duration("P"), None);
    assert_eq!(duration("PT"), None);
    assert_eq!(duration("P1H"), None);
}
