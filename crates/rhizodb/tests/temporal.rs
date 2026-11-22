mod support;

use rhizodb::Database;
use rhizodb::Date;
use rhizodb::ErrorKind;
use rhizodb::Parameters;
use rhizodb::Temporal;
use rhizodb::Value;
use support::*;

fn text(db: &Database, query: &str) -> String {
    match single(db, query) {
        Value::Temporal(value) => value.to_string(),
        other => panic!("{query} returned {other:?}"),
    }
}

#[test]
fn temporal_values_survive_being_stored_and_reopened() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("graph.redb");
    let stored = "datetime('2015-07-21T21:40:32.142+02:00[Europe/Stockholm]')";
    {
        let db = Database::open(&path).unwrap();
        run(
            &db,
            &format!(
                "CREATE (:V {{at: {stored}, on: date('2015-07-21'), took: duration('PT1.5S'), \
                 times: [localtime('12:00'), localtime('13:00')]}})"
            ),
        );
    }
    let db = Database::open(&path).unwrap();
    assert_eq!(
        text(&db, "MATCH (v:V) RETURN v.at"),
        "2015-07-21T21:40:32.142+02:00[Europe/Stockholm]"
    );
    assert_eq!(text(&db, "MATCH (v:V) RETURN v.took"), "PT1.5S");
    assert_eq!(
        single(&db, &format!("MATCH (v:V) RETURN v.at = {stored}")),
        boolean(true)
    );
    assert_eq!(
        single(&db, "MATCH (v:V) RETURN [t IN v.times | toString(t)]"),
        list([string("12:00"), string("13:00")])
    );
}

#[test]
fn a_temporal_parameter_is_read_as_the_value_it_holds() {
    let db = db();
    let date = Date::from_ymd(1984, 10, 11).unwrap();
    let mut parameters = Parameters::new();
    parameters.insert("d".to_owned(), Value::Temporal(Temporal::Date(date)));
    let result = db
        .execute(
            "RETURN $d.year AS year, $d + duration('P1M') AS later",
            &parameters,
        )
        .unwrap();
    assert_eq!(result.rows()[0][0], int(1984));
    assert_eq!(
        result.rows()[0][1],
        Value::Temporal(Temporal::Date(Date::from_ymd(1984, 11, 11).unwrap()))
    );
}

#[test]
fn temporal_values_sort_between_paths_and_strings() {
    let db = db();
    let rows = run(
        &db,
        "UNWIND ['b', date('2015-01-02'), 1, date('2015-01-01'), duration('P1D')] AS v \
         RETURN v ORDER BY v",
    );
    let sorted: Vec<String> = rows
        .iter()
        .map(|row| match &row[0] {
            Value::Temporal(value) => value.to_string(),
            other => format!("{other:?}"),
        })
        .collect();
    assert_eq!(
        sorted,
        [
            "2015-01-01",
            "2015-01-02",
            "P1D",
            "String(\"b\")",
            "Integer(1)"
        ]
    );
}

#[test]
fn values_of_different_temporal_types_do_not_compare() {
    let db = db();
    assert_eq!(
        single(
            &db,
            "RETURN date('2015-01-01') < localdatetime('2015-01-02')"
        ),
        Value::Null
    );
    assert_eq!(
        single(&db, "RETURN duration('P1D') < duration('P2D')"),
        Value::Null
    );
    assert_eq!(
        single(&db, "RETURN time('12:00+01:00') < time('12:00Z')"),
        boolean(true)
    );
}

#[test]
fn malformed_temporal_input_raises_its_own_error() {
    let db = db();
    for query in [
        "RETURN date('2015-13-01')",
        "RETURN datetime({year: 2015, timezone: 'Nowhere/City'})",
        "RETURN duration('P1H')",
        "RETURN date('2015-01-01').hour",
        "RETURN date.truncate('hour', date('2015-01-01'))",
    ] {
        assert_eq!(
            error_kind(&db, query),
            ErrorKind::InvalidTemporalValue,
            "{query}"
        );
    }
    assert_eq!(
        error_kind(&db, "RETURN duration('P1D') / 0"),
        ErrorKind::DivisionByZero
    );
}

#[test]
fn the_clock_functions_read_one_instant_per_query() {
    let db = db();
    assert_eq!(
        single(
            &db,
            "RETURN datetime() = datetime.transaction() AND date() = date.statement()"
        ),
        boolean(true)
    );
}

#[test]
fn a_duration_divided_by_a_float_is_exact() {
    let db = db();
    for (query, expected) in [
        ("RETURN duration('P1D') / 3.0", "PT8H"),
        ("RETURN duration('P1D') / 3", "PT8H"),
        ("RETURN duration('PT1S') / 0.3", "PT3.333333333S"),
        ("RETURN duration('P1M') / -0.5", "P-2M"),
        ("RETURN duration('PT1S') / 1e-9", "PT277777H46M40S"),
    ] {
        assert_eq!(text(&db, query), expected, "{query}");
    }
}

#[test]
fn dividing_by_a_tiny_float_gives_any_result_that_fits() {
    let db = db();
    for (query, expected) in [
        (
            "RETURN (duration('PT0.5S') / 1.2345678901234567e-15).seconds",
            "Integer(405000003645000)",
        ),
        ("RETURN duration('PT0S') / 1e-40", "PT0S"),
        ("RETURN duration('P1M') / 1e-300", "error"),
    ] {
        let result = db.execute(query, &Parameters::new());
        let actual = match result {
            Ok(result) => match &result.rows()[0][0] {
                Value::Temporal(value) => value.to_string(),
                other => format!("{other:?}"),
            },
            Err(error) => {
                assert_eq!(error.kind(), ErrorKind::InvalidTemporalValue, "{query}");
                "error".to_owned()
            }
        };
        assert_eq!(actual, expected, "{query}");
    }
}

#[test]
fn duration_amounts_with_many_digits_keep_the_ones_that_matter() {
    let db = db();
    for (query, expected) in [
        (
            "RETURN duration('PT0.12345678901234567890123456789012345678901S')",
            "PT0.123456789S",
        ),
        (
            "RETURN duration('PT0.12345678901234567890123456789012345678H')",
            "PT7M24.444440444S",
        ),
        (
            "RETURN duration('PT123456789.0123456789012345678901234567890123S')",
            "PT34293H33M9.012345678S",
        ),
    ] {
        assert_eq!(text(&db, query), expected, "{query}");
    }
}

#[test]
fn a_date_moves_by_the_whole_days_of_the_time_truncated_toward_zero() {
    let db = db();
    for (query, expected) in [
        (
            "RETURN date('2020-01-02') + duration({seconds: -86399.5})",
            "2020-01-02",
        ),
        (
            "RETURN date('2020-01-02') - duration({seconds: 86399.5})",
            "2020-01-02",
        ),
        (
            "RETURN date('2020-01-02') + duration({seconds: -86400.5})",
            "2020-01-01",
        ),
        (
            "RETURN date('2020-01-02') + duration({hours: 47, minutes: 59})",
            "2020-01-03",
        ),
    ] {
        assert_eq!(text(&db, query), expected, "{query}");
    }
}

#[test]
fn duration_strings_read_their_amounts_exactly() {
    let db = db();
    assert_eq!(
        single(&db, "RETURN duration('PT9007199254740993S').seconds"),
        int(9_007_199_254_740_993)
    );
    assert_eq!(
        text(&db, "RETURN duration('PT0.1000000019S')"),
        "PT0.100000001S"
    );
    assert_eq!(text(&db, "RETURN duration('P0,5D')"), "PT12H");
}

#[test]
fn the_same_instant_at_different_offsets_compares_equal_but_is_not_equal() {
    let db = db();
    for (query, expected) in [
        (
            "RETURN datetime('2015-07-21T12:00+01:00') > datetime('2015-07-21T11:00Z')",
            false,
        ),
        (
            "RETURN datetime('2015-07-21T12:00+01:00') <= datetime('2015-07-21T11:00Z')",
            true,
        ),
        (
            "RETURN datetime('2015-07-21T13:00+02:00[Europe/Stockholm]') >= \
             datetime('2015-07-21T11:00Z')",
            true,
        ),
        ("RETURN time('12:00+01:00') >= time('11:00Z')", true),
        ("RETURN time('12:00+01:00') < time('11:00Z')", false),
        (
            "RETURN datetime('2015-07-21T12:00+01:00') = datetime('2015-07-21T11:00Z')",
            false,
        ),
        ("RETURN time('12:00+01:00') = time('11:00Z')", false),
    ] {
        assert_eq!(single(&db, query), boolean(expected), "{query}");
    }
}
