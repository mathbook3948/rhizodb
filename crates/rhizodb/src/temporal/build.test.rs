use super::*;

fn now() -> DateTime {
    DateTime::from_instant(0, 0, &Zone::UTC).unwrap()
}

fn integer(key: &str, value: i64) -> (String, Input) {
    (key.to_owned(), Input::Integer(value))
}

fn text(key: &str, value: &str) -> (String, Input) {
    (key.to_owned(), Input::Text(value.to_owned()))
}

fn built(kind: Kind, fields: &[(String, Input)]) -> String {
    match from_fields(kind, fields, &now()) {
        Ok(value) => value.to_string(),
        Err(error) => format!("error: {error}"),
    }
}

#[test]
fn dates_build_from_every_kind_of_component() {
    let year = || integer("year", 1984);
    assert_eq!(
        built(
            Kind::Date,
            &[year(), integer("week", 10), integer("dayOfWeek", 3)]
        ),
        "1984-03-07"
    );
    assert_eq!(
        built(Kind::Date, &[year(), integer("ordinalDay", 202)]),
        "1984-07-20"
    );
    assert_eq!(
        built(
            Kind::Date,
            &[year(), integer("quarter", 3), integer("dayOfQuarter", 45)]
        ),
        "1984-08-14"
    );
    assert_eq!(
        built(Kind::Date, &[integer("year", 1817), integer("week", 1)]),
        "1816-12-30"
    );
    assert!(built(Kind::Date, &[integer("month", 2)]).starts_with("error"));
    assert!(built(Kind::Date, &[year(), integer("month", 13)]).starts_with("error"));
}

#[test]
fn base_values_supply_the_components_left_out() {
    let base = Temporal::Date(Date::from_ymd(1984, 11, 11).unwrap());
    let fields =
        |extra: (String, Input)| vec![("date".to_owned(), Input::Temporal(base.clone())), extra];
    assert_eq!(built(Kind::Date, &fields(integer("week", 1))), "1984-01-08");
    assert_eq!(
        built(Kind::Date, &fields(integer("quarter", 3))),
        "1984-08-11"
    );
    assert_eq!(
        built(Kind::Date, &fields(integer("year", 28))),
        "0028-11-11"
    );
}

#[test]
fn zoned_bases_convert_to_a_new_zone_at_the_same_instant() {
    let base = from_fields(
        Kind::DateTime,
        &[
            integer("year", 1984),
            integer("month", 10),
            integer("day", 11),
            integer("hour", 12),
            text("timezone", "Europe/Stockholm"),
        ],
        &now(),
    )
    .unwrap();
    assert_eq!(base.to_string(), "1984-10-11T12:00+01:00[Europe/Stockholm]");
    let fields = [
        ("datetime".to_owned(), Input::Temporal(base)),
        integer("day", 28),
        integer("second", 42),
        text("timezone", "Pacific/Honolulu"),
    ];
    assert_eq!(
        built(Kind::DateTime, &fields),
        "1984-10-28T01:00:42-10:00[Pacific/Honolulu]"
    );
}

#[test]
fn truncation_keeps_the_local_time_and_adds_sub_seconds() {
    let value = parsed(Kind::LocalDateTime, "1984-10-11T12:31:14.645876123").unwrap();
    let fields = [integer("nanosecond", 2)];
    let result = truncated(Kind::DateTime, "millisecond", &value, &fields, &now()).unwrap();
    assert_eq!(result.to_string(), "1984-10-11T12:31:14.645000002Z");
    let date = parsed(Kind::Date, "1984-01-01").unwrap();
    let fields = [text("timezone", "Europe/Stockholm")];
    let result = truncated(Kind::DateTime, "weekYear", &date, &fields, &now()).unwrap();
    assert_eq!(
        result.to_string(),
        "1983-01-03T00:00+01:00[Europe/Stockholm]"
    );
}

#[test]
fn durations_build_from_fractional_amounts() {
    let duration = |fields: &[(String, Input)]| duration_from_fields(fields).unwrap().to_string();
    let months = ("months".to_owned(), Input::Float(0.75));
    assert_eq!(duration(&[months]), "P22DT19H51M49.5S");
    let minutes = ("minutes".to_owned(), Input::Float(1.5));
    assert_eq!(duration(&[minutes, integer("seconds", 1)]), "PT1M31S");
}

#[test]
fn components_read_calendar_and_clock_fields() {
    let value = parsed(
        Kind::DateTime,
        "1984-11-11T12:31:14.645876123+01:00[Europe/Stockholm]",
    )
    .unwrap();
    assert_eq!(
        component(&value, "ordinalDay"),
        Some(Component::Integer(316))
    );
    assert_eq!(
        component(&value, "epochMillis"),
        Some(Component::Integer(469_020_674_645))
    );
    assert_eq!(
        component(&value, "timezone"),
        Some(Component::Text("Europe/Stockholm".to_owned()))
    );
    assert_eq!(
        component(&value, "offset"),
        Some(Component::Text("+01:00".to_owned()))
    );
    assert_eq!(component(&value, "years"), None);
}
