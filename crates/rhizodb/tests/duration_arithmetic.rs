mod support;

#[path = "support/exact.rs"]
mod exact;

use std::cmp::Ordering;
use std::sync::LazyLock;

use exact::Int;
use exact::Rational;
use proptest::prelude::*;
use rhizodb::Database;
use rhizodb::Date;
use rhizodb::DateTime;
use rhizodb::Duration;
use rhizodb::ErrorKind;
use rhizodb::LocalDateTime;
use rhizodb::LocalTime;
use rhizodb::Parameters;
use rhizodb::Temporal;
use rhizodb::Time;
use rhizodb::Value;
use support::*;

static DATABASE: LazyLock<Database> = LazyLock::new(db);

const NANOS_PER_SECOND: i128 = 1_000_000_000;
const NANOS_PER_DAY: i128 = 86_400 * NANOS_PER_SECOND;

fn evaluate(db: &Database, query: &str, parameters: &[(&str, Value)]) -> Result<Value, ErrorKind> {
    let parameters: Parameters = parameters
        .iter()
        .map(|(name, value)| ((*name).to_owned(), value.clone()))
        .collect();
    match db.execute(query, &parameters) {
        Ok(result) => Ok(result.rows()[0][0].clone()),
        Err(error) => Err(error.kind()),
    }
}

fn evaluate_duration(
    db: &Database,
    query: &str,
    parameters: &[(&str, Value)],
) -> Result<Duration, ErrorKind> {
    match evaluate(db, query, parameters)? {
        Value::Temporal(Temporal::Duration(duration)) => Ok(duration),
        other => panic!("{query} returned {other:?}"),
    }
}

fn duration_value(duration: Duration) -> Value {
    Value::Temporal(Temporal::Duration(duration))
}

fn total_nanos(duration: &Duration) -> i128 {
    i128::from(duration.seconds()) * NANOS_PER_SECOND + i128::from(duration.nanoseconds())
}

fn parts(duration: &Duration) -> (i64, i64, i64, u32) {
    (
        duration.months(),
        duration.days(),
        duration.seconds(),
        duration.nanoseconds(),
    )
}

const WHOLE_POOL: &[i64] = &[
    0,
    1,
    -1,
    2,
    -3,
    7,
    12,
    30,
    31,
    59,
    60,
    3600,
    86_399,
    86_400,
    1_000_000_000,
    -1_000_000_000,
    4_294_967_296,
    1_000_000_000_000_000_000,
    -1_000_000_000_000_000_000,
    i64::MAX,
    i64::MAX - 1,
    i64::MIN,
    i64::MIN + 1,
];

const NANOSECOND_POOL: &[i64] = &[
    0,
    1,
    -1,
    500_000_000,
    999_999_999,
    1_000_000_000,
    -999_999_999,
    -1_000_000_001,
    i64::MAX,
    i64::MIN,
];

fn whole() -> impl Strategy<Value = i64> {
    prop_oneof![
        3 => prop::sample::select(WHOLE_POOL),
        1 => any::<i64>(),
        1 => -100i64..100,
    ]
}

fn nanoseconds() -> impl Strategy<Value = i64> {
    prop_oneof![
        3 => prop::sample::select(NANOSECOND_POOL),
        1 => any::<i64>(),
        1 => 0i64..1_000_000_000,
    ]
}

fn duration() -> impl Strategy<Value = Duration> {
    (whole(), whole(), whole(), nanoseconds())
        .prop_filter_map("the duration is out of range", |(m, d, s, n)| {
            Duration::new(m, d, s, n)
        })
}

fn small_duration() -> impl Strategy<Value = Duration> {
    (
        -1000i64..1000,
        -1000i64..1000,
        -100_000i64..100_000,
        0i64..1_000_000_000,
    )
        .prop_filter_map("the duration is out of range", |(m, d, s, n)| {
            Duration::new(m, d, s, n)
        })
}

fn mixed_duration() -> impl Strategy<Value = Duration> {
    prop_oneof![3 => duration(), 2 => small_duration()]
}

const FLOAT_POOL: &[f64] = &[
    0.0,
    -0.0,
    1.0,
    -1.0,
    0.5,
    -0.5,
    2.5,
    7.0,
    0.1,
    0.3,
    -0.3,
    1.0 / 3.0,
    1e-9,
    1e-15,
    1.2345678901234567e-15,
    1e-23,
    1e-24,
    1.5e-24,
    1e-25,
    1e-30,
    1e-300,
    f64::MIN_POSITIVE,
    5e-324,
    1e9,
    1e18,
    9_007_199_254_740_993.0,
    i64::MAX as f64,
    1e20,
    1e24,
    1.0000000000000002e24,
    1e25,
    1e36,
    1e37,
    1e38,
    1e39,
    1e300,
    f64::MAX,
    f64::MIN,
    f64::INFINITY,
    f64::NEG_INFINITY,
    f64::NAN,
];

fn float_factor() -> impl Strategy<Value = f64> {
    prop_oneof![
        4 => prop::sample::select(FLOAT_POOL),
        2 => prop::num::f64::ANY,
        2 => (-1000i64..1000, -30i32..30).prop_map(|(digits, exponent)| digits as f64 * 10f64.powi(exponent)),
    ]
}

const INTEGER_POOL: &[i64] = &[
    0,
    1,
    -1,
    2,
    -2,
    3,
    7,
    86_400,
    1_000_000_000,
    1_000_000_000_000_000_000,
    i64::MAX,
    i64::MIN,
    i64::MIN + 1,
];

fn integer() -> impl Strategy<Value = i64> {
    prop_oneof![
        3 => prop::sample::select(INTEGER_POOL),
        1 => any::<i64>(),
        1 => -100i64..100,
    ]
}

struct Decimal {
    negative: bool,
    digits: u128,
    exponent: i32,
}

fn shortest_decimal(value: f64) -> Decimal {
    let text = format!("{value:e}");
    let (mantissa, exponent) = text.split_once('e').unwrap();
    let negative = mantissa.starts_with('-');
    let mantissa = mantissa.trim_start_matches('-');
    let fraction = mantissa
        .split_once('.')
        .map_or(0, |(_, fraction)| fraction.len());
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    Decimal {
        negative,
        digits: digits.parse().unwrap(),
        exponent: exponent.parse::<i32>().unwrap() - i32::try_from(fraction).unwrap(),
    }
}

fn signed(negative: bool, magnitude: Int) -> Int {
    if negative {
        magnitude.negated()
    } else {
        magnitude
    }
}

struct Scale {
    numerator: Int,
    denominator: Int,
}

fn multiplier(decimal: &Decimal) -> Scale {
    let places = decimal.exponent.unsigned_abs();
    let digits = Int::of(i128::try_from(decimal.digits).unwrap());
    let (magnitude, denominator) = if decimal.exponent >= 0 {
        (digits.times(&Int::power_of_ten(places)), Int::of(1))
    } else {
        (digits, Int::power_of_ten(places))
    };
    Scale {
        numerator: signed(decimal.negative, magnitude),
        denominator,
    }
}

fn divisor(decimal: &Decimal) -> Scale {
    let places = decimal.exponent.unsigned_abs();
    let digits = Int::of(i128::try_from(decimal.digits).unwrap());
    let (magnitude, denominator) = if decimal.exponent >= 0 {
        (Int::of(1), digits.times(&Int::power_of_ten(places)))
    } else {
        (Int::power_of_ten(places), digits)
    };
    Scale {
        numerator: signed(decimal.negative, magnitude),
        denominator,
    }
}

fn scaled(duration: &Duration, scale: &Scale) -> Option<Duration> {
    let unit = |count: i128| {
        Rational::new(
            Int::of(count).times(&scale.numerator),
            scale.denominator.clone(),
        )
    };
    let (months, rest) = unit(i128::from(duration.months())).split();
    let (days, rest) = unit(i128::from(duration.days()))
        .plus(&rest.times(48_699, 1_600))
        .split();
    let (seconds, rest) = unit(i128::from(duration.seconds()))
        .plus(&rest.times(86_400, 1))
        .split();
    let (nanoseconds, _) = unit(i128::from(duration.nanoseconds()))
        .plus(&rest.times(NANOS_PER_SECOND, 1))
        .split();
    let nanoseconds = nanoseconds.to_i128()?;
    let seconds = seconds
        .to_i128()?
        .checked_add(nanoseconds.div_euclid(NANOS_PER_SECOND))?;
    Duration::new(
        i64::try_from(months.to_i128()?).ok()?,
        i64::try_from(days.to_i128()?).ok()?,
        i64::try_from(seconds).ok()?,
        i64::try_from(nanoseconds.rem_euclid(NANOS_PER_SECOND)).ok()?,
    )
}

fn expect_scaled(
    actual: Result<Duration, ErrorKind>,
    expected: Option<Duration>,
    context: &str,
) -> Result<(), TestCaseError> {
    match (actual, expected) {
        (Ok(actual), Some(expected)) => {
            prop_assert_eq!(parts(&actual), parts(&expected), "{}", context);
        }
        (Err(kind), None) => {
            prop_assert_eq!(kind, ErrorKind::InvalidTemporalValue, "{}", context);
        }
        (actual, expected) => {
            prop_assert!(
                false,
                "{context}: got {actual:?}, exact result is {expected:?}"
            );
        }
    }
    Ok(())
}

fn expect_unchecked(
    actual: Result<Duration, ErrorKind>,
    context: &str,
) -> Result<(), TestCaseError> {
    if let Err(kind) = actual {
        prop_assert_eq!(kind, ErrorKind::InvalidTemporalValue, "{}", context);
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(6000))]

    #[test]
    #[ignore = "left out of mise run check, run by mise run random"]
    fn multiplying_by_a_float_matches_exact_arithmetic(duration in mixed_duration(), factor in float_factor()) {
        let db = &*DATABASE;
        let context = format!("{duration:?} * {factor:e}");
        let actual = evaluate_duration(
            db,
            "RETURN $d * $f",
            &[("d", duration_value(duration)), ("f", Value::Float(factor))],
        );
        if !factor.is_finite() {
            prop_assert_eq!(actual.map(|_| ()), Err(ErrorKind::InvalidTemporalValue), "{}", context);
            return Ok(());
        }
        let decimal = shortest_decimal(factor);
        if decimal.exponent >= -24 {
            expect_scaled(actual, scaled(&duration, &multiplier(&decimal)), &context)?;
        } else {
            expect_unchecked(actual, &context)?;
        }
    }

    #[test]
    #[ignore = "left out of mise run check, run by mise run random"]
    fn dividing_by_a_float_matches_exact_arithmetic(duration in mixed_duration(), divisor_value in float_factor()) {
        let db = &*DATABASE;
        let context = format!("{duration:?} / {divisor_value:e}");
        let actual = evaluate_duration(
            db,
            "RETURN $d / $f",
            &[("d", duration_value(duration)), ("f", Value::Float(divisor_value))],
        );
        if divisor_value.is_nan() {
            prop_assert_eq!(actual.map(|_| ()), Err(ErrorKind::InvalidTemporalValue), "{}", context);
        } else if divisor_value == 0.0 {
            prop_assert_eq!(actual.map(|_| ()), Err(ErrorKind::DivisionByZero), "{}", context);
        } else if divisor_value.is_infinite() {
            prop_assert_eq!(actual.map(|d| parts(&d)), Ok((0, 0, 0, 0)), "{}", context);
        } else if divisor_value.abs() <= 1e24 {
            let decimal = shortest_decimal(divisor_value);
            expect_scaled(actual, scaled(&duration, &divisor(&decimal)), &context)?;
        } else {
            prop_assert!(actual.is_ok(), "{}: {:?}", context, actual);
        }
    }

    #[test]
    #[ignore = "left out of mise run check, run by mise run random"]
    fn multiplying_by_an_integer_matches_exact_arithmetic(duration in mixed_duration(), factor in integer()) {
        let db = &*DATABASE;
        let context = format!("{duration:?} * {factor}");
        let scale = Scale { numerator: Int::of(i128::from(factor)), denominator: Int::of(1) };
        let expected = scaled(&duration, &scale);
        for query in ["RETURN $d * $f", "RETURN $f * $d"] {
            let actual = evaluate_duration(
                db,
                query,
                &[("d", duration_value(duration)), ("f", int(factor))],
            );
            expect_scaled(actual, expected, &context)?;
        }
    }

    #[test]
    #[ignore = "left out of mise run check, run by mise run random"]
    fn dividing_by_an_integer_matches_exact_arithmetic(duration in mixed_duration(), divisor_value in integer()) {
        let db = &*DATABASE;
        let context = format!("{duration:?} / {divisor_value}");
        let actual = evaluate_duration(
            db,
            "RETURN $d / $f",
            &[("d", duration_value(duration)), ("f", int(divisor_value))],
        );
        if divisor_value == 0 {
            prop_assert_eq!(actual.map(|_| ()), Err(ErrorKind::DivisionByZero), "{}", context);
            return Ok(());
        }
        let magnitude = Int::of(i128::from(divisor_value).abs());
        let scale = Scale {
            numerator: Int::of(i128::from(divisor_value.signum())),
            denominator: magnitude,
        };
        expect_scaled(actual, scaled(&duration, &scale), &context)?;
    }

    #[test]
    #[ignore = "left out of mise run check, run by mise run random"]
    fn a_zero_duration_scales_to_zero_whatever_the_finite_factor(factor in float_factor(), divisor_value in integer()) {
        let db = &*DATABASE;
        let zero = duration_value(Duration::new(0, 0, 0, 0).unwrap());
        let times = evaluate_duration(db, "RETURN $d * $f", &[("d", zero.clone()), ("f", Value::Float(factor))]);
        if factor.is_finite() {
            prop_assert_eq!(times.map(|d| parts(&d)), Ok((0, 0, 0, 0)), "0 * {:e}", factor);
        } else {
            prop_assert_eq!(times.map(|_| ()), Err(ErrorKind::InvalidTemporalValue), "0 * {:e}", factor);
        }
        let divided = evaluate_duration(db, "RETURN $d / $f", &[("d", zero.clone()), ("f", Value::Float(factor))]);
        if factor.is_nan() {
            prop_assert_eq!(divided.map(|_| ()), Err(ErrorKind::InvalidTemporalValue), "0 / {:e}", factor);
        } else if factor == 0.0 {
            prop_assert_eq!(divided.map(|_| ()), Err(ErrorKind::DivisionByZero), "0 / {:e}", factor);
        } else {
            prop_assert_eq!(divided.map(|d| parts(&d)), Ok((0, 0, 0, 0)), "0 / {:e}", factor);
        }
        if divisor_value != 0 {
            let divided = evaluate_duration(db, "RETURN $d / $f", &[("d", zero), ("f", int(divisor_value))]);
            prop_assert_eq!(divided.map(|d| parts(&d)), Ok((0, 0, 0, 0)), "0 / {}", divisor_value);
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(4000))]

    #[test]
    #[ignore = "left out of mise run check, run by mise run random"]
    fn a_duration_reads_back_the_string_it_prints(duration in duration(), factor in float_factor()) {
        let db = &*DATABASE;
        let scaled_by_query = evaluate_duration(
            db,
            "RETURN $d * $f",
            &[("d", duration_value(duration)), ("f", Value::Float(factor))],
        );
        for candidate in std::iter::once(duration).chain(scaled_by_query.ok()) {
            let printed = evaluate(db, "RETURN toString($d)", &[("d", duration_value(candidate))]).unwrap();
            prop_assert_eq!(&printed, &string(&candidate.to_string()));
            let Value::String(printed) = printed else { unreachable!() };
            let read = evaluate_duration(db, "RETURN duration($s)", &[("s", string(&printed))]);
            prop_assert_eq!(read.map(|d| parts(&d)), Ok(parts(&candidate)), "{}", printed);
        }
    }
}

fn digits(length: std::ops::RangeInclusive<usize>) -> impl Strategy<Value = String> {
    prop::collection::vec(prop_oneof![3 => Just('0'), 3 => Just('9'), 4 => (0u8..10).prop_map(|digit| char::from(b'0' + digit))], length)
        .prop_map(|digits| digits.into_iter().collect())
}

fn whole_digits() -> impl Strategy<Value = String> {
    prop_oneof![
        3 => digits(1..=18),
        1 => Just("9223372036854775807".to_owned()),
        1 => Just("9223372036854775808".to_owned()),
        1 => Just("9223372036854775809".to_owned()),
        1 => Just("0".to_owned()),
    ]
}

fn digit_value(text: &str) -> Int {
    text.chars().fold(Int::of(0), |value, digit| {
        value
            .times(&Int::of(10))
            .plus(&Int::of(i128::from(digit.to_digit(10).unwrap())))
    })
}

fn expected_seconds_text(
    negative: bool,
    whole: &str,
    fraction: &str,
    seconds_per_unit: i128,
) -> Option<Duration> {
    let scale = Int::power_of_ten(u32::try_from(fraction.len()).unwrap());
    let amount = digit_value(whole)
        .times(&scale)
        .plus(&digit_value(fraction))
        .times(&Int::of(seconds_per_unit * NANOS_PER_SECOND));
    let (nanoseconds, _) = amount.divided_toward_zero(&scale);
    let nanoseconds = nanoseconds.to_i128()?;
    let nanoseconds = if negative { -nanoseconds } else { nanoseconds };
    Duration::new(
        0,
        0,
        i64::try_from(nanoseconds.div_euclid(NANOS_PER_SECOND)).ok()?,
        i64::try_from(nanoseconds.rem_euclid(NANOS_PER_SECOND)).ok()?,
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(4000))]

    #[test]
    #[ignore = "left out of mise run check, run by mise run random"]
    fn a_duration_string_keeps_its_whole_part_and_every_nanosecond(
        negative in any::<bool>(),
        whole in whole_digits(),
        fraction in digits(0..=40),
        comma in any::<bool>(),
        unit in prop::sample::select(&[('S', 1i128), ('M', 60), ('H', 3600)][..]),
    ) {
        let db = &*DATABASE;
        let sign = if negative { "-" } else { "" };
        let separator = if fraction.is_empty() { "" } else if comma { "," } else { "." };
        let text = format!("PT{sign}{whole}{separator}{fraction}{}", unit.0);
        let actual = evaluate_duration(db, "RETURN duration($s)", &[("s", string(&text))]);
        let exact_digits = if unit.0 == 'S' { fraction.len() } else { fraction.len().min(24) };
        if unit.0 != 'S' && fraction.len() > 24 {
            expect_unchecked(actual, &text)?;
            return Ok(());
        }
        let expected = expected_seconds_text(negative, &whole, &fraction[..exact_digits], unit.1);
        expect_scaled(actual, expected, &text)?;
    }
}

const YEAR_POOL: &[i64] = &[
    -999_999_999,
    -999_999_998,
    -400,
    -1,
    0,
    1,
    1900,
    1969,
    1970,
    2000,
    2015,
    2020,
    9999,
    10_000,
    999_999_998,
    999_999_999,
];

fn date() -> impl Strategy<Value = Date> {
    (
        prop_oneof![3 => prop::sample::select(YEAR_POOL), 1 => -999_999_999i64..=999_999_999],
        prop_oneof![3 => 1u32..=12, 1 => prop::sample::select(&[2u32, 3, 10, 11][..])],
        prop_oneof![3 => 1u32..=28, 2 => prop::sample::select(&[1u32, 8, 25, 29, 30, 31][..])],
    )
        .prop_filter_map("no such date", |(year, month, day)| {
            Date::from_ymd(year, month, day)
        })
}

fn local_time() -> impl Strategy<Value = LocalTime> {
    (
        prop_oneof![3 => 0u32..24, 1 => prop::sample::select(&[0u32, 1, 2, 3, 23][..])],
        prop_oneof![3 => 0u32..60, 1 => prop::sample::select(&[0u32, 59, 30][..])],
        prop_oneof![3 => 0u32..60, 1 => prop::sample::select(&[0u32, 59][..])],
        prop_oneof![
            3 => prop::sample::select(&[0u32, 1, 500_000_000, 999_999_999][..]),
            1 => 0u32..1_000_000_000,
        ],
    )
        .prop_map(|(hour, minute, second, nanosecond)| {
            LocalTime::from_hms_nano(hour, minute, second, nanosecond).unwrap()
        })
}

const OFFSET_POOL: &[i32] = &[
    0, 1, -1, 3600, -3600, 19_800, 64_800, -64_800, 63_999, -45_296,
];

fn offset() -> impl Strategy<Value = i32> {
    prop_oneof![3 => prop::sample::select(OFFSET_POOL), 1 => -64_800i32..=64_800]
}

const ZONES: &[&str] = &[
    "Europe/Stockholm",
    "America/New_York",
    "Asia/Kolkata",
    "Pacific/Apia",
    "Australia/Lord_Howe",
    "UTC",
];

fn local_date_time() -> impl Strategy<Value = LocalDateTime> {
    (date(), local_time()).prop_map(|(date, time)| LocalDateTime::new(date, time))
}

fn date_time() -> impl Strategy<Value = DateTime> {
    prop_oneof![
        (local_date_time(), offset()).prop_filter_map("out of range", |(local, offset)| {
            DateTime::with_offset(local, offset)
        }),
        (local_date_time(), prop::sample::select(ZONES))
            .prop_filter_map("out of range", |(local, zone)| {
                DateTime::in_zone(local, zone)
            }),
    ]
}

fn shifted_time(time: LocalTime, seconds: i64) -> Option<LocalTime> {
    let total = i64::from(time.hour()) * 3600
        + i64::from(time.minute()) * 60
        + i64::from(time.second())
        + seconds;
    let total = u32::try_from(total).ok().filter(|total| *total < 86_400)?;
    LocalTime::from_hms_nano(total / 3600, total / 60 % 60, total % 60, time.nanosecond())
}

fn pair() -> impl Strategy<Value = (Temporal, Temporal)> {
    let dates = (date(), date()).prop_map(|(a, b)| (Temporal::Date(a), Temporal::Date(b)));
    let local_times = (local_time(), local_time())
        .prop_map(|(a, b)| (Temporal::LocalTime(a), Temporal::LocalTime(b)));
    let times = (local_time(), offset(), local_time(), offset()).prop_filter_map(
        "out of range",
        |(a, a_offset, b, b_offset)| {
            Some((
                Temporal::Time(Time::new(a, a_offset)?),
                Temporal::Time(Time::new(b, b_offset)?),
            ))
        },
    );
    let same_instant_times = (local_time(), offset(), offset()).prop_filter_map(
        "no same instant",
        |(a, a_offset, b_offset)| {
            let b = shifted_time(a, i64::from(a_offset) - i64::from(b_offset))?;
            Some((
                Temporal::Time(Time::new(a, a_offset)?),
                Temporal::Time(Time::new(b, b_offset)?),
            ))
        },
    );
    let local_date_times = (local_date_time(), local_date_time())
        .prop_map(|(a, b)| (Temporal::LocalDateTime(a), Temporal::LocalDateTime(b)));
    let date_times = (date_time(), date_time())
        .prop_map(|(a, b)| (Temporal::DateTime(a), Temporal::DateTime(b)));
    let same_instant_date_times = (
        date(),
        local_time(),
        prop_oneof![
            offset().prop_map(|offset| (None, offset)),
            prop::sample::select(ZONES).prop_map(|zone| (Some(zone), 0))
        ],
        offset(),
    )
        .prop_filter_map(
            "no same instant",
            |(date, time, (zone, offset_a), offset_b)| {
                let local = LocalDateTime::new(date, time);
                let a = match zone {
                    Some(zone) => DateTime::in_zone(local, zone)?,
                    None => DateTime::with_offset(local, offset_a)?,
                };
                let shifted =
                    shifted_time(time, i64::from(a.offset_seconds()) - i64::from(offset_b))?;
                let b = DateTime::with_offset(LocalDateTime::new(date, shifted), offset_b)?;
                Some((Temporal::DateTime(a), Temporal::DateTime(b)))
            },
        );
    prop_oneof![
        dates,
        local_times,
        times,
        same_instant_times,
        local_date_times,
        date_times,
        same_instant_date_times,
    ]
}

fn sorted(db: &Database, first: &Temporal, second: &Temporal) -> Vec<Value> {
    match evaluate(
        db,
        "UNWIND [$a, $b] AS v WITH v ORDER BY v RETURN collect(v)",
        &[
            ("a", Value::Temporal(first.clone())),
            ("b", Value::Temporal(second.clone())),
        ],
    ) {
        Ok(Value::List(values)) => values,
        other => panic!("sorting returned {other:?}"),
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(3000))]

    #[test]
    #[ignore = "left out of mise run check, run by mise run random"]
    fn comparison_agrees_with_the_difference_and_not_with_order_by((a, b) in pair()) {
        let db = &*DATABASE;
        let context = format!("{a} vs {b}");
        let parameters = [("a", Value::Temporal(a.clone())), ("b", Value::Temporal(b.clone()))];
        let difference = evaluate_duration(db, "RETURN duration.inSeconds($a, $b)", &parameters).unwrap();
        let ordering = total_nanos(&difference).cmp(&0);
        for (operator, holds) in [
            ("<", ordering == Ordering::Greater),
            ("<=", ordering != Ordering::Less),
            (">", ordering == Ordering::Less),
            (">=", ordering != Ordering::Greater),
        ] {
            let actual = evaluate(db, &format!("RETURN $a {operator} $b"), &parameters).unwrap();
            prop_assert_eq!(actual, boolean(holds), "{} {} (difference {})", context, operator, difference);
        }
        let backwards = sorted(db, &b, &a);
        let forwards = sorted(db, &a, &b);
        if a != b && ordering == Ordering::Equal {
            prop_assert_eq!(&forwards, &backwards, "{}: ORDER BY ties", context);
        }
        if ordering != Ordering::Equal {
            let (low, high) = if ordering == Ordering::Greater { (&a, &b) } else { (&b, &a) };
            prop_assert_eq!(
                forwards,
                vec![Value::Temporal(low.clone()), Value::Temporal(high.clone())],
                "{}",
                context
            );
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(4000))]

    #[test]
    #[ignore = "left out of mise run check, run by mise run random"]
    fn a_date_moves_by_months_then_days_then_the_whole_days_of_the_time(
        date in date(),
        duration in mixed_duration(),
    ) {
        let db = &*DATABASE;
        let context = format!("{date} with {duration:?}");
        let date_value = Value::Temporal(Temporal::Date(date));
        let whole_days = i64::try_from(total_nanos(&duration) / NANOS_PER_DAY).unwrap();
        let chained = |months: i64, days: i64, whole_days: i64| {
            evaluate(
                db,
                "RETURN (($date + $months) + $days) + $whole",
                &[
                    ("date", date_value.clone()),
                    ("months", duration_value(Duration::new(months, 0, 0, 0).unwrap())),
                    ("days", duration_value(Duration::new(0, days, 0, 0).unwrap())),
                    ("whole", duration_value(Duration::new(0, whole_days, 0, 0).unwrap())),
                ],
            )
        };
        let expected = chained(duration.months(), duration.days(), whole_days);
        let added = evaluate(
            db,
            "RETURN $date + $d",
            &[("date", date_value.clone()), ("d", duration_value(duration))],
        );
        prop_assert_eq!(&added, &expected, "{} +", context);
        let negated = (|| {
            let total = -total_nanos(&duration);
            Duration::new(
                duration.months().checked_neg()?,
                duration.days().checked_neg()?,
                i64::try_from(total.div_euclid(NANOS_PER_SECOND)).ok()?,
                i64::try_from(total.rem_euclid(NANOS_PER_SECOND)).ok()?,
            )
        })();
        let subtracted = evaluate(
            db,
            "RETURN $date - $d",
            &[("date", date_value.clone()), ("d", duration_value(duration))],
        );
        match negated {
            Some(negated) => {
                let whole = i64::try_from(total_nanos(&negated) / NANOS_PER_DAY).unwrap();
                prop_assert_eq!(subtracted, chained(negated.months(), negated.days(), whole), "{} -", context);
            }
            None => prop_assert_eq!(subtracted, Err(ErrorKind::InvalidTemporalValue), "{} -", context),
        }
    }
}
