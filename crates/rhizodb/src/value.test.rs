use super::*;
use crate::temporal::calendar::MAX_DAYS;
use crate::temporal::calendar::MIN_DAYS;
use crate::temporal::calendar::NANOS_PER_DAY;
use crate::temporal::calendar::NANOS_PER_SECOND;
use crate::temporal::zone::MAX_OFFSET_SECONDS;
use proptest::prelude::*;

const ZONES: [&str; 4] = [
    "Europe/Stockholm",
    "America/New_York",
    "Asia/Kolkata",
    "UTC",
];

fn temporal() -> impl Strategy<Value = Temporal> {
    let date = MIN_DAYS..=MAX_DAYS;
    let time = 0..NANOS_PER_DAY;
    let offset = -MAX_OFFSET_SECONDS..=MAX_OFFSET_SECONDS;
    let instant = -50_000_000_000i64..50_000_000_000;
    let nanosecond = 0..NANOS_PER_SECOND as u32;
    prop_oneof![
        date.clone()
            .prop_filter_map("date", |days| Date::from_days(days).map(Temporal::Date)),
        time.clone()
            .prop_filter_map("time", |nanos| LocalTime::from_nanos(nanos)
                .map(Temporal::LocalTime)),
        (time.clone(), offset.clone()).prop_filter_map("time", |(nanos, offset)| {
            LocalTime::from_nanos(nanos)
                .and_then(|time| Time::new(time, offset))
                .map(Temporal::Time)
        }),
        (date, time).prop_filter_map("local datetime", |(days, nanos)| {
            let date = Date::from_days(days)?;
            let time = LocalTime::from_nanos(nanos)?;
            Some(Temporal::LocalDateTime(LocalDateTime::new(date, time)))
        }),
        (instant.clone(), nanosecond.clone(), offset).prop_filter_map(
            "datetime",
            |(second, nanosecond, offset)| {
                DateTime::from_stored(second, nanosecond, offset, None).map(Temporal::DateTime)
            }
        ),
        (instant, nanosecond.clone(), 0..ZONES.len()).prop_filter_map(
            "zoned datetime",
            |(second, nanosecond, zone)| {
                DateTime::from_stored(second, nanosecond, 0, Some(ZONES[zone]))
                    .map(Temporal::DateTime)
            }
        ),
        (any::<i64>(), any::<i64>(), any::<i64>(), nanosecond).prop_filter_map(
            "duration",
            |(months, days, seconds, nanoseconds)| {
                Duration::from_stored(months, days, seconds, nanoseconds).map(Temporal::Duration)
            }
        ),
    ]
}

pub(crate) fn property_value() -> impl Strategy<Value = PropertyValue> {
    let leaf = prop_oneof![
        any::<bool>().prop_map(PropertyValue::Boolean),
        any::<i64>().prop_map(PropertyValue::Integer),
        any::<u64>().prop_map(|bits| PropertyValue::Float(f64::from_bits(bits))),
        any::<String>().prop_map(PropertyValue::String),
        prop::collection::vec(any::<u8>(), 0..32).prop_map(PropertyValue::Bytes),
        temporal().prop_map(PropertyValue::Temporal),
    ];
    leaf.prop_recursive(4, 32, 6, |inner| {
        prop::collection::vec(inner, 0..6).prop_map(PropertyValue::List)
    })
}

pub(crate) fn same_bits(left: &PropertyValue, right: &PropertyValue) -> bool {
    match (left, right) {
        (PropertyValue::Float(a), PropertyValue::Float(b)) => a.to_bits() == b.to_bits(),
        (PropertyValue::List(a), PropertyValue::List(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| same_bits(x, y))
        }
        (a, b) => a == b,
    }
}

proptest! {
    #[test]
    fn every_value_round_trips_bit_exactly(value in property_value()) {
        let decoded = PropertyValue::decode(&value.encode()).unwrap();
        prop_assert!(same_bits(&value, &decoded));
    }

    #[test]
    fn skipping_a_list_of_runs_of_one_type_lands_on_the_next_value(
        runs in prop::collection::vec((property_value(), 1usize..12), 0..6),
        cut in any::<prop::sample::Index>(),
    ) {
        let items = runs
            .into_iter()
            .flat_map(|(value, count)| std::iter::repeat_n(value, count))
            .collect();
        let mut bytes = PropertyValue::List(items).encode();
        let list_end = bytes.len();
        PropertyValue::Integer(7).encode_into(&mut bytes);
        let mut reader = Reader::new(&bytes);
        reader.skip_value().unwrap();
        prop_assert_eq!(reader.value(), Ok(PropertyValue::Integer(7)));
        let mut truncated = Reader::new(&bytes[..cut.index(list_end)]);
        prop_assert_eq!(truncated.skip_value(), Err(DecodeError::UnexpectedEnd));
    }

    #[test]
    fn arbitrary_bytes_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..64)) {
        let _ = PropertyValue::decode(&bytes);
    }
}

#[test]
fn nan_and_negative_zero_keep_their_bits() {
    for bits in [
        f64::NAN.to_bits(),
        (-0.0f64).to_bits(),
        0x7ff8_0000_0000_0001,
    ] {
        let encoded = PropertyValue::Float(f64::from_bits(bits)).encode();
        match PropertyValue::decode(&encoded).unwrap() {
            PropertyValue::Float(value) => assert_eq!(value.to_bits(), bits),
            other => panic!("expected a float, got {other:?}"),
        }
    }
}

#[test]
fn empty_input_is_rejected() {
    assert_eq!(PropertyValue::decode(&[]), Err(DecodeError::UnexpectedEnd));
}

#[test]
fn unknown_tag_is_rejected() {
    assert_eq!(
        PropertyValue::decode(&[99]),
        Err(DecodeError::UnknownTag(99))
    );
}

proptest! {
    #[test]
    fn a_list_of_single_precision_floats_round_trips_bit_exactly(
        bits in prop::collection::vec(any::<u32>(), 0..80),
    ) {
        let value = PropertyValue::List(
            bits.iter()
                .map(|bits| PropertyValue::Float(f64::from(f32::from_bits(*bits))))
                .collect(),
        );
        let decoded = PropertyValue::decode(&value.encode()).unwrap();
        prop_assert!(same_bits(&value, &decoded));
    }
}

#[test]
fn a_list_of_single_precision_floats_takes_four_bytes_a_float() {
    let floats = |values: &[f64]| {
        PropertyValue::List(values.iter().copied().map(PropertyValue::Float).collect())
    };
    let embedding: Vec<f64> = (0..64).map(|i| f64::from(i - 32) / 64.0).collect();
    assert_eq!(floats(&embedding).encode().len(), 2 + 64 * 4);
    let mut mixed = embedding.clone();
    mixed.push(0.1);
    assert_eq!(floats(&mixed).encode().len(), 2 + 65 * 9);
}

#[test]
fn a_list_of_floats_stored_one_by_one_still_reads() {
    let mut bytes = vec![TAG_LIST, 2, TAG_FLOAT];
    bytes.extend_from_slice(&0.5f64.to_bits().to_be_bytes());
    bytes.push(TAG_FLOAT);
    bytes.extend_from_slice(&(-2.0f64).to_bits().to_be_bytes());
    assert_eq!(
        PropertyValue::decode(&bytes),
        Ok(PropertyValue::List(vec![
            PropertyValue::Float(0.5),
            PropertyValue::Float(-2.0)
        ]))
    );
}

#[test]
fn truncated_integer_is_rejected() {
    assert_eq!(
        PropertyValue::decode(&[TAG_INTEGER, 0, 0]),
        Err(DecodeError::UnexpectedEnd)
    );
}

#[test]
fn trailing_bytes_are_rejected() {
    let mut bytes = PropertyValue::Boolean(true).encode();
    bytes.push(0);
    assert_eq!(
        PropertyValue::decode(&bytes),
        Err(DecodeError::TrailingBytes)
    );
}

#[test]
fn boolean_byte_other_than_zero_or_one_is_rejected() {
    assert_eq!(
        PropertyValue::decode(&[TAG_BOOLEAN, 2]),
        Err(DecodeError::InvalidBoolean(2))
    );
}

#[test]
fn invalid_utf8_is_rejected() {
    assert_eq!(
        PropertyValue::decode(&[TAG_STRING, 1, 0xff]),
        Err(DecodeError::InvalidUtf8)
    );
}

#[test]
fn a_huge_declared_length_does_not_allocate_or_panic() {
    let mut list = vec![TAG_LIST];
    put_varint(&mut list, usize::MAX);
    assert_eq!(
        PropertyValue::decode(&list),
        Err(DecodeError::UnexpectedEnd)
    );

    let mut overlong = vec![TAG_BYTES];
    overlong.extend_from_slice(&[0xff; 9]);
    overlong.push(0x7f);
    assert_eq!(
        PropertyValue::decode(&overlong),
        Err(DecodeError::LengthOverflow)
    );
}

#[test]
fn nesting_beyond_the_limit_is_rejected_without_overflowing_the_stack() {
    let mut bytes = Vec::new();
    for _ in 0..10_000 {
        bytes.push(TAG_LIST);
        bytes.push(1);
    }
    assert_eq!(
        PropertyValue::decode(&bytes),
        Err(DecodeError::NestingTooDeep)
    );
}

#[test]
fn temporal_values_out_of_range_are_rejected() {
    let mut bytes = vec![TAG_LOCAL_TIME];
    bytes.extend_from_slice(&NANOS_PER_DAY.to_be_bytes());
    assert_eq!(
        PropertyValue::decode(&bytes),
        Err(DecodeError::InvalidTemporal)
    );
    let mut bytes = vec![TAG_ZONED_DATETIME];
    bytes.extend_from_slice(&0i64.to_be_bytes());
    bytes.extend_from_slice(&0u32.to_be_bytes());
    put_varint(&mut bytes, 4);
    bytes.extend_from_slice(b"Nope");
    assert_eq!(
        PropertyValue::decode(&bytes),
        Err(DecodeError::InvalidTemporal)
    );
}

#[test]
fn a_zoned_datetime_keeps_its_zone_name() {
    let value = DateTime::from_stored(469_020_674, 645_876_123, 0, Some("Europe/Stockholm"))
        .map(Temporal::DateTime)
        .unwrap();
    let decoded = PropertyValue::decode(&PropertyValue::Temporal(value.clone()).encode()).unwrap();
    assert_eq!(decoded, PropertyValue::Temporal(value));
    assert_eq!(
        decoded,
        PropertyValue::Temporal(
            crate::temporal::build::parsed(
                crate::temporal::build::Kind::DateTime,
                "1984-11-11T12:31:14.645876123+01:00[Europe/Stockholm]"
            )
            .unwrap()
        )
    );
}
