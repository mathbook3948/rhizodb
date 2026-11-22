use super::*;
use proptest::prelude::*;

fn property_value() -> impl Strategy<Value = PropertyValue> {
    let leaf = prop_oneof![
        any::<bool>().prop_map(PropertyValue::Boolean),
        any::<i64>().prop_map(PropertyValue::Integer),
        any::<u64>().prop_map(|bits| PropertyValue::Float(f64::from_bits(bits))),
        any::<String>().prop_map(PropertyValue::String),
        prop::collection::vec(any::<u8>(), 0..32).prop_map(PropertyValue::Bytes),
    ];
    leaf.prop_recursive(4, 32, 6, |inner| {
        prop::collection::vec(inner, 0..6).prop_map(PropertyValue::List)
    })
}

fn same_bits(left: &PropertyValue, right: &PropertyValue) -> bool {
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
