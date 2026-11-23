use std::collections::BTreeMap;
use std::collections::BTreeSet;

use proptest::prelude::*;
use proptest::test_runner::TestCaseError;

use super::*;
use crate::value::tests::property_value;
use crate::value::tests::same_bits;

const GROUP: u64 = 3;

const SEGMENT: usize = 5;

fn token() -> Token<PropertyKey> {
    Token::new(7)
}

fn key() -> SegmentKey {
    segment_key(GROUP, token(), SEGMENT, 0)
}

fn is_corruption(error: &GraphError) -> bool {
    matches!(error, GraphError::Corrupt(_) | GraphError::Decode(_))
}

fn encode_parts(values: &[(usize, PropertyValue)]) -> Vec<Part> {
    let cells: Vec<(usize, Cell<'_>)> = values
        .iter()
        .map(|(at, value)| (*at, Cell::of(value)))
        .collect();
    encode_segment(GROUP, token(), SEGMENT, &cells).unwrap()
}

fn encode(values: &[(usize, PropertyValue)]) -> Option<Vec<u8>> {
    let mut parts = encode_parts(values);
    assert!(
        parts.len() <= 1,
        "the segment is split into {} parts",
        parts.len()
    );
    let (held, bytes) = parts.pop()?;
    assert_eq!(held, key());
    Some(bytes)
}

fn kind_of(segment: &[u8]) -> Kind {
    Kind::from_tag(segment[KIND_AT] & KIND_BITS).unwrap()
}

fn span_in(segment: &[u8]) -> usize {
    usize::from(segment[SPAN_AT])
}

fn read_back(segment: &[u8]) -> Result<Vec<(usize, PropertyValue)>, GraphError> {
    read_parts(&[(key(), segment.to_vec())])
}

fn read_parts(parts: &[Part]) -> Result<Vec<(usize, PropertyValue)>, GraphError> {
    let mut read = Vec::new();
    for (key, bytes) in parts {
        for cell in SegmentRef::parse(key, bytes)?.cells() {
            let (at, cell) = cell?;
            read.push((at, cell.to_value()?));
        }
    }
    Ok(read)
}

fn same_values(left: &[(usize, PropertyValue)], right: &[(usize, PropertyValue)]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|((a, x), (b, y))| a == b && same_bits(x, y))
}

fn assert_reads_back(values: &[(usize, PropertyValue)]) -> Result<Vec<Part>, TestCaseError> {
    let parts = encode_parts(values);
    let read = read_parts(&parts).unwrap();
    prop_assert!(same_values(&read, values), "{:?} != {:?}", read, values);
    prop_assert_eq!(
        parts.first().map(|(key, _)| *key),
        Some(key()),
        "part 0 is stored"
    );
    let span = span_in(&parts[0].1);
    let views: Vec<SegmentRef<'_>> = parts
        .iter()
        .map(|(key, bytes)| SegmentRef::parse(key, bytes).unwrap())
        .collect();
    for ((held, bytes), view) in parts.iter().zip(&views) {
        prop_assert_eq!(&held[..held.len() - 1], &key()[..key().len() - 1]);
        prop_assert_eq!(span_in(bytes), span, "every part records one span");
        prop_assert!(
            bytes.len() <= PART_BYTES || view.cells().count() == 1,
            "a part of {} bytes",
            bytes.len()
        );
    }
    let cells: Vec<(usize, Cell<'_>)> = values
        .iter()
        .map(|(at, value)| (*at, Cell::of(value)))
        .collect();
    for wider in span + 1..=SEGMENT_SLOTS {
        prop_assert!(
            parts_of(&cells, wider).any(|(_, part)| part_bytes(wider, part) > PART_BYTES),
            "parts of {} positions would fit",
            wider
        );
    }
    let mut column = Column::new();
    for (key, bytes) in &parts {
        column.insert(SEGMENT, Segment::load(key, bytes).unwrap());
    }
    let mut expected = values.iter().peekable();
    for at in 0..SEGMENT_SLOTS {
        let want = expected
            .next_if(|(held, _)| *held == at)
            .map(|(_, value)| value);
        let part = views[0].part_of(at);
        let point = parts
            .iter()
            .position(|(key, _)| part_of_key(key) == part)
            .and_then(|index| views[index].cell_at(at).unwrap())
            .map(|cell| cell.to_value().unwrap());
        let loaded = column
            .cell_at(SEGMENT * SEGMENT_SLOTS + at)
            .unwrap()
            .map(|cell| cell.to_value().unwrap());
        for found in [point, loaded] {
            match (&found, want) {
                (Some(found), Some(want)) => prop_assert!(same_bits(found, want), "{}", at),
                (None, None) => {}
                _ => prop_assert!(false, "position {}: {:?} != {:?}", at, found, want),
            }
        }
    }
    for slot in [
        0,
        SEGMENT * SEGMENT_SLOTS - 1,
        (SEGMENT + 1) * SEGMENT_SLOTS,
        SLOTS - 1,
    ] {
        prop_assert!(column.cell_at(slot).unwrap().is_none(), "slot {}", slot);
    }
    prop_assert!(encode_parts(&read) == parts, "the encoding is canonical");
    Ok(parts)
}

fn kinds_of(parts: &[Part]) -> BTreeSet<u8> {
    parts
        .iter()
        .filter(|(key, bytes)| SegmentRef::parse(key, bytes).unwrap().cells().count() > 0)
        .map(|(_, bytes)| bytes[KIND_AT] & KIND_BITS)
        .collect()
}

fn typed_value(kind: u8) -> BoxedStrategy<PropertyValue> {
    match kind % 4 {
        0 => any::<bool>().prop_map(PropertyValue::Boolean).boxed(),
        1 => any::<i64>().prop_map(PropertyValue::Integer).boxed(),
        2 => any::<u64>()
            .prop_map(|bits| PropertyValue::Float(f64::from_bits(bits)))
            .boxed(),
        _ => any::<String>().prop_map(PropertyValue::String).boxed(),
    }
}

fn segment_values(kind: u8) -> impl Strategy<Value = Vec<(usize, PropertyValue)>> {
    let value = if kind < 4 {
        typed_value(kind)
    } else {
        property_value().boxed()
    };
    prop::collection::btree_set(0..SEGMENT_SLOTS, 1..=SEGMENT_SLOTS).prop_flat_map(move |ats| {
        let count = ats.len();
        let ats: Vec<usize> = ats.into_iter().collect();
        prop::collection::vec(value.clone(), count)
            .prop_map(move |values| ats.iter().copied().zip(values).collect::<Vec<_>>())
    })
}

fn full(
    value: impl Fn(usize) -> PropertyValue,
    ats: impl Iterator<Item = usize>,
) -> Vec<(usize, PropertyValue)> {
    ats.map(|at| (at, value(at))).collect()
}

type Entries = Vec<(Token<PropertyKey>, Option<PropertyValue>)>;

fn delta_entries() -> impl Strategy<Value = Entries> {
    prop::collection::btree_map(any::<u32>(), prop::option::of(property_value()), 1..12).prop_map(
        |entries| {
            entries
                .into_iter()
                .map(|(token, value)| (Token::new(token), value))
                .collect()
        },
    )
}

fn encode_entries(
    key: &[u8; DELTA_KEY],
    entries: &[(Token<PropertyKey>, Option<PropertyValue>)],
) -> Vec<u8> {
    encode_delta(
        key,
        entries
            .iter()
            .map(|(token, value)| (*token, value.as_ref().map(Cell::of))),
    )
}

fn delta_read_back(key: &[u8], bytes: &[u8]) -> Result<Entries, GraphError> {
    DeltaRef::parse(key, bytes)?
        .entries()
        .map(|entry| {
            let (token, cell) = entry?;
            Ok((token, cell.map(Cell::to_value).transpose()?))
        })
        .collect()
}

fn same_entries(left: &Entries, right: &Entries) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|((a, x), (b, y))| {
            a == b
                && match (x, y) {
                    (Some(x), Some(y)) => same_bits(x, y),
                    (None, None) => true,
                    _ => false,
                }
        })
}

proptest! {
    #[test]
    fn a_segment_of_one_type_reads_back_every_value_from_every_position(
        values in (0u8..4).prop_flat_map(segment_values),
    ) {
        let parts = assert_reads_back(&values)?;
        let expected = match values[0].1 {
            PropertyValue::Boolean(_) => Kind::Bool,
            PropertyValue::Integer(_) => Kind::Int,
            PropertyValue::Float(_) => Kind::Float,
            _ => Kind::Str,
        };
        prop_assert_eq!(kinds_of(&parts), BTreeSet::from([expected.tag()]));
    }

    #[test]
    fn a_segment_of_mixed_types_reads_back_every_value_from_every_position(
        values in segment_values(4),
    ) {
        let parts = assert_reads_back(&values)?;
        for (key, bytes) in &parts {
            let held: Vec<(usize, Cell<'_>)> = SegmentRef::parse(key, bytes)
                .unwrap()
                .cells()
                .collect::<Result<_, _>>()
                .unwrap();
            let kinds: BTreeSet<u8> = held.iter().map(|(_, cell)| cell.kind().tag()).collect();
            let expected = match kinds.len() {
                1 => held[0].1.kind(),
                _ => Kind::Variant,
            };
            prop_assert_eq!(kind_of(bytes), expected);
        }
    }

    #[test]
    fn a_delta_reads_back_each_property_and_answers_for_each_key(
        slot in 0..SLOTS,
        entries in delta_entries(),
        probe in any::<u32>(),
    ) {
        let key = delta_key(GROUP, slot);
        let bytes = encode_entries(&key, &entries);
        let read = delta_read_back(&key, &bytes).unwrap();
        prop_assert!(same_entries(&read, &entries), "{:?} != {:?}", read, entries);
        let loaded = Delta::load(&key, &bytes).unwrap();
        let tokens: Vec<Token<PropertyKey>> =
            loaded.view().tokens().collect::<Result<_, _>>().unwrap();
        let expected: Vec<Token<PropertyKey>> = entries.iter().map(|(token, _)| *token).collect();
        prop_assert_eq!(tokens, expected);
        let probes = entries
            .iter()
            .map(|(token, _)| *token)
            .chain([Token::new(probe), Token::new(0), Token::new(u32::MAX)]);
        for token in probes {
            let want = entries.iter().find(|(held, _)| *held == token).map(|(_, value)| value);
            let found = loaded
                .view()
                .get(token)
                .unwrap()
                .map(|cell| cell.map(|cell| cell.to_value().unwrap()));
            match (&found, want) {
                (Some(Some(found)), Some(Some(want))) => prop_assert!(same_bits(found, want)),
                (Some(None), Some(None)) | (None, None) => {}
                _ => prop_assert!(false, "{:?}: {:?} != {:?}", token, found, want),
            }
        }
    }

    #[test]
    fn merged_values_hold_each_change_over_the_stored_value(
        stored in segment_values(1),
        changes in prop::collection::btree_map(
            0..SEGMENT_SLOTS,
            prop::option::of(any::<i64>()),
            0..SEGMENT_SLOTS,
        ),
    ) {
        let segment = encode(&stored).unwrap();
        let view = SegmentRef::parse(&key(), &segment).unwrap();
        let changes: BTreeMap<usize, Option<PropertyValue>> = changes
            .into_iter()
            .map(|(at, value)| (at, value.map(PropertyValue::Integer)))
            .collect();
        let listed: Vec<(usize, Option<Cell<'_>>)> = changes
            .iter()
            .map(|(at, value)| (*at, value.as_ref().map(Cell::of)))
            .collect();
        let cells = merged(view.cells(), &listed).unwrap();
        let mut expected: BTreeMap<usize, PropertyValue> = stored.iter().cloned().collect();
        for (at, value) in &changes {
            match value {
                Some(value) => expected.insert(*at, value.clone()),
                None => expected.remove(at),
            };
        }
        let found: Vec<(usize, PropertyValue)> = cells
            .iter()
            .map(|(at, cell)| (*at, cell.to_value().unwrap()))
            .collect();
        let expected: Vec<(usize, PropertyValue)> = expected.into_iter().collect();
        prop_assert!(same_values(&found, &expected));
    }
}

#[test]
fn segments_of_every_size_at_byte_and_segment_edges_read_back() {
    let makers: [fn(usize) -> PropertyValue; 5] = [
        |at| PropertyValue::Boolean(at % 3 == 0),
        |at| PropertyValue::Integer(at as i64 * 7 - 9000),
        |at| PropertyValue::Float(at as f64 / 3.0),
        |at| PropertyValue::String("s".repeat(at % 5)),
        |at| {
            if at % 2 == 0 {
                PropertyValue::Integer(at as i64)
            } else {
                PropertyValue::List(vec![PropertyValue::String(at.to_string())])
            }
        },
    ];
    for make in makers {
        for count in [1, 2, 7, 8, 9, 15, 16, 17, 31, 32, 33, 63, 64] {
            assert_reads_back(&full(make, 0..count)).unwrap();
            assert_reads_back(&full(make, SEGMENT_SLOTS - count..SEGMENT_SLOTS)).unwrap();
        }
        assert_reads_back(&full(make, (0..SEGMENT_SLOTS).step_by(8))).unwrap();
        assert_reads_back(&full(make, (7..SEGMENT_SLOTS).step_by(8))).unwrap();
        assert_reads_back(&full(make, [0, SEGMENT_SLOTS - 1].into_iter())).unwrap();
    }
}

#[test]
fn edge_values_of_each_type_read_back_bit_for_bit() {
    let values = [
        PropertyValue::Integer(i64::MIN),
        PropertyValue::Integer(i64::MAX),
        PropertyValue::Integer(0),
        PropertyValue::Integer(-1),
    ];
    let values: Vec<(usize, PropertyValue)> = values.into_iter().enumerate().collect();
    let parts = assert_reads_back(&values).unwrap();
    assert_eq!(kinds_of(&parts), BTreeSet::from([Kind::Int.tag()]));
    let floats = [
        -0.0,
        0.0,
        f64::NAN,
        -f64::NAN,
        f64::from_bits(0x7ff0_0000_0000_0001),
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::MIN_POSITIVE,
        f64::from_bits(1),
        f64::MAX,
    ];
    let values: Vec<(usize, PropertyValue)> = floats
        .iter()
        .map(|value| PropertyValue::Float(*value))
        .enumerate()
        .collect();
    let parts = assert_reads_back(&values).unwrap();
    assert_eq!(kinds_of(&parts), BTreeSet::from([Kind::Float.tag()]));
    let long = "long".repeat(5000);
    let strings = ["", "a", "ünïcödé ✓", "\0", long.as_str()];
    let values: Vec<(usize, PropertyValue)> = strings
        .iter()
        .map(|text| PropertyValue::String((*text).to_owned()))
        .enumerate()
        .collect();
    let parts = assert_reads_back(&values).unwrap();
    assert_eq!(kinds_of(&parts), BTreeSet::from([Kind::Str.tag()]));
    let only_empty = [(SEGMENT_SLOTS - 1, PropertyValue::String(String::new()))];
    assert_reads_back(&only_empty).unwrap();
    let segment = encode(&only_empty).unwrap();
    assert_eq!(
        segment.len(),
        values_at(SEGMENT_SLOTS) + 2,
        "one end offset and no heap"
    );
    let bools = full(|at| PropertyValue::Boolean(at % 2 == 0), 0..9);
    assert_reads_back(&bools).unwrap();
    let segment = encode(&bools).unwrap();
    assert_eq!(
        segment.len(),
        values_at(SEGMENT_SLOTS) + 2,
        "nine values in two bytes"
    );
}

#[test]
fn a_segment_without_values_is_not_stored() {
    assert_eq!(encode(&[]), None);
}

#[test]
fn a_segment_too_large_for_one_part_splits_into_the_widest_parts_that_fit() {
    let text = |length: usize| move |_: usize| PropertyValue::String("x".repeat(length));
    let whole = assert_reads_back(&full(text(10), 0..SEGMENT_SLOTS)).unwrap();
    assert_eq!(whole.len(), 1, "64 short strings fit one part");
    assert_eq!(span_in(&whole[0].1), SEGMENT_SLOTS);
    let two = assert_reads_back(&full(text(20), 0..SEGMENT_SLOTS)).unwrap();
    assert_eq!(two.len(), 2);
    assert_eq!(two[1].0, segment_key(GROUP, token(), SEGMENT, 1));
    assert_eq!(
        span_in(&two[0].1),
        44,
        "44 strings of 20 bytes fit a part and 45 do not"
    );
    assert_eq!(
        read_parts(&two[1..]).unwrap().len(),
        20,
        "the last part ends with the segment"
    );
    let singles = assert_reads_back(&full(text(2000), 0..SEGMENT_SLOTS)).unwrap();
    assert_eq!(
        singles.len(),
        SEGMENT_SLOTS,
        "a value over the limit takes a part of its own"
    );
    assert_eq!(span_in(&singles[0].1), 1);
    let late = assert_reads_back(&full(text(2000), [40, 63].into_iter())).unwrap();
    assert_eq!(
        late.len(),
        3,
        "part 0 is stored without values to record the span"
    );
    assert_eq!(late[0].0, key());
    assert_eq!(late[0].1.len(), values_at(1));
    let mixed: Vec<(usize, PropertyValue)> = (0..SEGMENT_SLOTS)
        .map(|at| match at {
            0..32 => (at, PropertyValue::Integer(at as i64)),
            _ => (at, PropertyValue::String("y".repeat(200))),
        })
        .collect();
    let parts = assert_reads_back(&mixed).unwrap();
    assert_eq!(
        kind_of(&parts[0].1),
        Kind::Int,
        "each part takes the type of its own values"
    );
}

#[test]
fn a_resealed_part_that_holds_a_position_outside_it_or_no_value_fails_as_corruption() {
    let parts = encode_parts(&full(|_| PropertyValue::String("z".repeat(600)), 0..4));
    assert_eq!(parts.len(), 4);
    let reseal_under = |key: &[u8], bytes: &mut Vec<u8>| {
        let sum = crc(&[key, &bytes[KIND_AT..]]);
        bytes[CHECKSUM_AT..KIND_AT].copy_from_slice(&sum.to_le_bytes());
    };
    let (key1, part1) = &parts[1];
    let mut outside = part1.clone();
    outside[BITMAP_AT] = 0b10;
    reseal_under(key1, &mut outside);
    let respan = |span: u8| {
        let mut bytes = part1.clone();
        bytes[SPAN_AT] = span;
        reseal_under(key1, &mut bytes);
        bytes
    };
    let wider = respan(SEGMENT_SLOTS as u8);
    let no_span = respan(0);
    let unknown = respan(SEGMENT_SLOTS as u8 + 1);
    let mut flags = part1.clone();
    flags[KIND_AT] |= 0x10;
    reseal_under(key1, &mut flags);
    let last_key = segment_key(GROUP, token(), SEGMENT, 2);
    let mut past_end = encode_part(&last_key, 30, &[(SEGMENT_SLOTS - 1, Cell::Int(1))]).unwrap();
    past_end[BITMAP_AT + 1] |= 0x01;
    reseal_under(&last_key, &mut past_end);
    let empty_key = segment_key(GROUP, token(), SEGMENT, 9);
    let mut empty = encode_part(&empty_key, 1, &[]).unwrap();
    reseal_under(&empty_key, &mut empty);
    let whole_empty = encode_part(&key(), SEGMENT_SLOTS, &[]).unwrap();
    for (name, key, bytes) in [
        ("outside", key1, &outside),
        ("wider", key1, &wider),
        ("no span", key1, &no_span),
        ("unknown span", key1, &unknown),
        ("unknown flags", key1, &flags),
        ("position past the segment", &last_key, &past_end),
        ("empty part beyond 0", &empty_key, &empty),
        ("empty whole segment", &key(), &whole_empty),
    ] {
        assert!(
            SegmentRef::parse(key, bytes).is_err_and(|error| is_corruption(&error)),
            "{name}"
        );
    }
    assert!(SegmentRef::parse(&parts[0].0, &parts[0].1).is_ok());
}

#[test]
fn a_segment_takes_positions_inside_it_in_ascending_order() {
    let one = PropertyValue::Integer(1);
    for cells in [
        vec![(SEGMENT_SLOTS, Cell::of(&one))],
        vec![(3, Cell::of(&one)), (2, Cell::of(&one))],
        vec![(3, Cell::of(&one)), (3, Cell::of(&one))],
    ] {
        assert!(
            encode_segment(GROUP, token(), SEGMENT, &cells)
                .is_err_and(|error| is_corruption(&error))
        );
    }
}

#[test]
fn values_in_their_tagged_encoding_store_as_the_typed_segment_they_would_be() {
    let values = [
        PropertyValue::Integer(5),
        PropertyValue::String("x".to_owned()),
        PropertyValue::Boolean(true),
        PropertyValue::Float(-0.0),
    ];
    for value in values {
        let mut tagged = Vec::new();
        value.encode_into(&mut tagged);
        let encoded = encode_segment(GROUP, token(), SEGMENT, &[(9, Cell::Encoded(&tagged))])
            .unwrap()
            .remove(0)
            .1;
        let typed = encode(&[(9, value.clone())]).unwrap();
        assert_eq!(kind_of(&encoded), Kind::Variant);
        assert_ne!(kind_of(&typed), Kind::Variant);
        let read = read_back(&encoded).unwrap();
        assert!(same_values(&read, &[(9, value)]));
        assert_eq!(
            encode(&read).unwrap(),
            typed,
            "reading it back gives the typed segment"
        );
    }
}

#[test]
fn cells_differ_by_type_and_bits_and_agree_otherwise() {
    let list = PropertyValue::List(vec![PropertyValue::Integer(1)]);
    let other = PropertyValue::List(vec![PropertyValue::Integer(2)]);
    let mut tagged = Vec::new();
    list.encode_into(&mut tagged);
    let pairs: [(Cell<'_>, Cell<'_>, bool); 10] = [
        (Cell::Float(0.0), Cell::Float(-0.0), false),
        (Cell::Float(f64::NAN), Cell::Float(f64::NAN), true),
        (Cell::Int(1), Cell::Float(1.0), false),
        (Cell::Int(1), Cell::Int(1), true),
        (Cell::Bool(true), Cell::Int(1), false),
        (Cell::Str(""), Cell::Str(""), true),
        (Cell::Str("a"), Cell::Str("b"), false),
        (Cell::Value(&list), Cell::Encoded(&tagged), true),
        (Cell::Value(&list), Cell::Value(&other), false),
        (Cell::Str("1"), Cell::Value(&list), false),
    ];
    for (left, right, same) in pairs {
        assert_eq!(left.same(&right), same, "{left:?} {right:?}");
        assert_eq!(right.same(&left), same, "{right:?} {left:?}");
    }
    assert!(same_state(None, None));
    assert!(!same_state(Some(Cell::Int(0)), None));
    assert!(!same_state(None, Some(Cell::Int(0))));
}

fn sample_segments() -> Vec<(&'static str, Vec<(usize, PropertyValue)>)> {
    vec![
        (
            "bool",
            full(
                |at| PropertyValue::Boolean(at % 2 == 1),
                [1, 5, 9, 63].into_iter(),
            ),
        ),
        (
            "int",
            full(|at| PropertyValue::Integer(at as i64 - 3), 0..40),
        ),
        (
            "float",
            full(|at| PropertyValue::Float(at as f64 * 0.5), 10..64),
        ),
        (
            "string",
            full(|at| PropertyValue::String(format!("v{at}")), 0..64),
        ),
        (
            "variant",
            full(
                |at| match at % 3 {
                    0 => PropertyValue::Integer(at as i64),
                    1 => PropertyValue::String(format!("s{at}")),
                    _ => PropertyValue::List(vec![PropertyValue::Boolean(true)]),
                },
                0..50,
            ),
        ),
    ]
}

#[test]
fn a_changed_byte_anywhere_in_a_segment_fails_its_check_and_never_misreads_a_value() {
    for (name, values) in sample_segments() {
        let segment = encode(&values).unwrap();
        for at in 0..segment.len() {
            for flip in [0x01u8, 0x80, 0xff] {
                let mut changed = segment.clone();
                changed[at] ^= flip;
                let whole = read_back(&changed);
                assert!(
                    whole.as_ref().is_err_and(is_corruption),
                    "{name}: byte {at} ^ {flip:#x} read as {whole:?}"
                );
                assert!(
                    Segment::load(&key(), &changed).is_err_and(|error| is_corruption(&error)),
                    "{name}: byte {at} ^ {flip:#x} loads"
                );
            }
        }
    }
}

#[test]
fn a_cut_padded_or_emptied_segment_fails_its_check() {
    for (name, values) in sample_segments() {
        let segment = encode(&values).unwrap();
        let mut padded = segment.clone();
        padded.push(0);
        let changes = [
            segment[..segment.len() - 1].to_vec(),
            segment[..segment.len() / 2].to_vec(),
            segment[..values_at(SEGMENT_SLOTS)].to_vec(),
            segment[..1].to_vec(),
            Vec::new(),
            padded,
        ];
        for changed in changes {
            let result = read_back(&changed);
            assert!(
                result.as_ref().is_err_and(is_corruption),
                "{name}: {} bytes",
                changed.len()
            );
        }
    }
}

#[test]
fn a_segment_read_under_another_key_fails_its_check() {
    let segment = encode(&[(0, PropertyValue::Integer(1))]).unwrap();
    for other in [
        segment_key(GROUP + 1, token(), SEGMENT, 0),
        segment_key(GROUP, Token::new(8), SEGMENT, 0),
        segment_key(GROUP, token(), SEGMENT + 1, 0),
        segment_key(0, Token::new(0), 0, 0),
    ] {
        let error = SegmentRef::parse(&other, &segment).unwrap_err();
        assert!(is_corruption(&error));
    }
}

fn reseal(segment: &mut [u8]) {
    let sum = crc(&[&key()[..], &segment[KIND_AT..]]);
    segment[CHECKSUM_AT..KIND_AT].copy_from_slice(&sum.to_le_bytes());
}

#[test]
fn a_resealed_segment_of_an_unknown_layout_type_or_count_fails_as_corruption() {
    let segment = encode(&full(|at| PropertyValue::Integer(at as i64), 0..3)).unwrap();
    let mut layout = segment.clone();
    layout[0] = LAYOUT_VERSION + 1;
    let mut kind = segment.clone();
    kind[KIND_AT] = (kind[KIND_AT] & !KIND_BITS) | 7;
    let mut empty = segment.clone();
    empty[BITMAP_AT..values_at(SEGMENT_SLOTS)].copy_from_slice(&0u64.to_le_bytes());
    let mut more = segment.clone();
    more[BITMAP_AT] |= 0x08;
    let mut fewer = segment.clone();
    fewer[BITMAP_AT] &= !0x04;
    for (name, mut changed) in [
        ("layout", layout),
        ("kind", kind),
        ("empty", empty),
        ("more", more),
        ("fewer", fewer),
    ] {
        reseal(&mut changed);
        let result = read_back(&changed);
        assert!(
            result.as_ref().is_err_and(is_corruption),
            "{name}: {result:?}"
        );
    }
}

#[test]
fn a_resealed_segment_whose_offsets_or_text_are_wrong_still_fails_as_corruption() {
    let values = full(|at| PropertyValue::String(format!("v{at}")), 0..40);
    let segment = encode(&values).unwrap();
    let heap = values_at(SEGMENT_SLOTS) + 2 * values.len();
    let mut descending = segment.clone();
    let at = values_at(SEGMENT_SLOTS) + 2 * 3;
    descending[at..at + 2].copy_from_slice(&1u16.to_le_bytes());
    let mut outside = segment.clone();
    let last = values_at(SEGMENT_SLOTS) + 2 * 39;
    outside[last..last + 2].copy_from_slice(&u16::MAX.to_le_bytes());
    let mut not_utf8 = segment.clone();
    not_utf8[heap] = 0xff;
    for (name, mut changed) in [
        ("descending offsets", descending),
        ("offset outside", outside),
        ("not utf-8", not_utf8),
    ] {
        reseal(&mut changed);
        let result = read_back(&changed);
        assert!(
            result.as_ref().is_err_and(is_corruption),
            "{name}: {result:?}"
        );
    }
}

#[test]
fn a_resealed_variant_segment_with_an_undecodable_value_fails_where_it_is_read() {
    let values = full(
        |at| PropertyValue::List(vec![PropertyValue::Integer(at as i64)]),
        0..4,
    );
    let mut segment = encode(&values).unwrap();
    segment[values_at(SEGMENT_SLOTS) + 2 * values.len()] = 0xee;
    reseal(&mut segment);
    assert!(read_back(&segment).is_err_and(|error| is_corruption(&error)));
    let view = SegmentRef::parse(&key(), &segment).unwrap();
    assert!(view.cell_at(0).is_err_and(|error| is_corruption(&error)));
    assert!(view.cell_at(1).unwrap().unwrap().to_value().is_ok());
}

fn sample_deltas() -> Vec<Entries> {
    vec![
        vec![(token(), None)],
        vec![(token(), Some(PropertyValue::Integer(-4)))],
        vec![
            (
                Token::new(0),
                Some(PropertyValue::String("text".to_owned())),
            ),
            (token(), None),
            (
                Token::new(u32::MAX),
                Some(PropertyValue::List(vec![PropertyValue::Float(0.5)])),
            ),
        ],
    ]
}

#[test]
fn a_changed_cut_or_padded_delta_or_one_under_another_key_fails_its_check() {
    let key = delta_key(GROUP, 17);
    for entries in sample_deltas() {
        let bytes = encode_entries(&key, &entries);
        for at in 0..bytes.len() {
            for flip in [0x01u8, 0x80, 0xff] {
                let mut changed = bytes.clone();
                changed[at] ^= flip;
                let result = delta_read_back(&key, &changed);
                assert!(
                    result.as_ref().is_err_and(is_corruption),
                    "{entries:?} byte {at}"
                );
                assert!(Delta::load(&key, &changed).is_err_and(|error| is_corruption(&error)));
            }
        }
        for cut in 0..bytes.len() {
            assert!(delta_read_back(&key, &bytes[..cut]).is_err_and(|error| is_corruption(&error)));
        }
        let mut padded = bytes.clone();
        padded.push(0);
        assert!(delta_read_back(&key, &padded).is_err_and(|error| is_corruption(&error)));
        for other in [
            delta_key(GROUP, 18),
            delta_key(GROUP + 1, 17),
            delta_key(0, 0),
        ] {
            assert!(delta_read_back(&other, &bytes).is_err_and(|error| is_corruption(&error)));
        }
        assert!(
            delta_read_back(&segment_key(GROUP, token(), 0, 0), &bytes)
                .is_err_and(|error| is_corruption(&error))
        );
    }
}

fn seal_delta(key: &[u8], body: &[u8]) -> Vec<u8> {
    let mut bytes = vec![LAYOUT_VERSION];
    bytes.extend_from_slice(body);
    let sum = crc(&[key, &bytes]);
    bytes.extend_from_slice(&sum.to_le_bytes());
    bytes
}

fn entry(token: u32, value: Option<&PropertyValue>) -> Vec<u8> {
    let mut bytes = token.to_be_bytes().to_vec();
    match value {
        Some(value) => {
            bytes.push(PRESENT);
            value.encode_into(&mut bytes);
        }
        None => bytes.push(ABSENT),
    }
    bytes
}

#[test]
fn a_resealed_delta_that_is_empty_unordered_or_holds_no_state_fails_as_corruption() {
    let key = delta_key(GROUP, 2);
    let three = PropertyValue::Integer(3);
    let mut unknown_state = 5u32.to_be_bytes().to_vec();
    unknown_state.push(2);
    let mut no_value = 5u32.to_be_bytes().to_vec();
    no_value.push(PRESENT);
    let mut trailing = entry(5, Some(&three));
    trailing.push(0);
    let bodies = [
        ("empty", Vec::new()),
        ("unknown state", unknown_state),
        ("no value", no_value),
        ("trailing byte", trailing),
        ("cut key", vec![0, 0, 5]),
        ("descending", [entry(6, None), entry(5, None)].concat()),
        (
            "repeated",
            [entry(5, None), entry(5, Some(&three))].concat(),
        ),
    ];
    for (name, body) in bodies {
        let bytes = seal_delta(&key, &body);
        let result = delta_read_back(&key, &bytes);
        assert!(
            result.as_ref().is_err_and(is_corruption),
            "{name}: {result:?}"
        );
        assert!(Delta::load(&key, &bytes).is_err(), "{name}");
    }
    let mut layout = seal_delta(&key, &entry(5, None));
    layout[0] = LAYOUT_VERSION + 1;
    let end = layout.len() - 4;
    let sum = crc(&[&key, &layout[..end]]);
    layout[end..].copy_from_slice(&sum.to_le_bytes());
    assert!(delta_read_back(&key, &layout).is_err_and(|error| is_corruption(&error)));
    let good = seal_delta(&key, &[entry(1, None), entry(5, Some(&three))].concat());
    assert_eq!(
        good,
        encode_entries(&key, &[(Token::new(1), None), (Token::new(5), Some(three))])
    );
}

#[test]
fn a_resealed_delta_with_an_undecodable_value_fails_where_it_is_read() {
    let key = delta_key(GROUP, 2);
    let text = PropertyValue::String("ok".to_owned());
    let mut body = [entry(1, Some(&text)), entry(5, Some(&text))].concat();
    body[4 + 1 + 1 + 1] = 0xff;
    let bytes = seal_delta(&key, &body);
    let delta = Delta::load(&key, &bytes).unwrap();
    assert!(
        delta
            .view()
            .get(Token::new(1))
            .is_err_and(|error| is_corruption(&error))
    );
    assert!(delta.view().get(Token::new(5)).unwrap().unwrap().is_some());
    assert!(delta_read_back(&key, &bytes).is_err_and(|error| is_corruption(&error)));
}

proptest! {
    #[test]
    fn segment_keys_sort_as_their_group_key_segment_and_part_and_read_back(
        left in (any::<u64>(), any::<u32>(), 0..SEGMENTS, 0..SEGMENT_SLOTS),
        right in (any::<u64>(), any::<u32>(), 0..SEGMENTS, 0..SEGMENT_SLOTS),
        small in (0u64..70_000, 0u32..300),
    ) {
        let key = |(group, token, segment, part): (u64, u32, usize, usize)| {
            segment_key(group, Token::new(token), segment, part)
        };
        let small = (small.0, small.1, 0, 0);
        for one in [left, right, small] {
            let (group, token, segment, part) = one;
            prop_assert_eq!(
                parse_key(&key(one)).unwrap(),
                ColumnKey::Segment { group, token: Token::new(token), segment, part }
            );
            prop_assert!(key(one)[..] > delta_key(u64::MAX, SLOTS - 1)[..]);
            prop_assert!(key(one) < segments_end(group));
            prop_assert!(key(one).starts_with(&column_prefix(group, Token::new(token))));
            prop_assert!(key(one).starts_with(&parts_prefix(group, Token::new(token), segment)));
        }
        prop_assert_eq!(key(left).cmp(&key(right)), left.cmp(&right));
        prop_assert_eq!(
            segments_end(left.0).cmp(&key(right)),
            if left.0 < right.0 { std::cmp::Ordering::Less } else { std::cmp::Ordering::Greater }
        );
        prop_assert!(key(small).len() <= 1 + 4 + 3 + 2, "small numbers take few bytes");
    }
}

#[test]
fn a_segment_key_that_spells_a_number_with_a_leading_zero_or_too_many_bytes_fails() {
    let good = segment_key(258, token(), 1, 2).to_vec();
    assert_eq!(good, vec![SEGMENT_KIND, 2, 1, 2, 1, 7, 1, 2]);
    for bad in [
        vec![SEGMENT_KIND, 3, 0, 1, 2, 1, 7, 1, 2],
        vec![SEGMENT_KIND, 2, 1, 2, 2, 0, 7, 1, 2],
        vec![SEGMENT_KIND, 9, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 1, 2],
        vec![SEGMENT_KIND, 0, 5, 1, 1, 1, 1, 1, 1, 2],
        vec![SEGMENT_KIND, 2, 1, 2, 1, 7, 1],
        vec![SEGMENT_KIND, 2, 1, 2, 1, 7, 1, 2, 0],
        vec![SEGMENT_KIND, 2, 1],
        vec![SEGMENT_KIND],
    ] {
        assert!(
            parse_key(&bad).is_err_and(|error| is_corruption(&error)),
            "{bad:?}"
        );
    }
}

#[test]
fn keys_name_their_segment_or_delta_and_others_fail() {
    for (segment, part) in [(0, 0), (1, 3), (SEGMENTS - 1, SEGMENT_SLOTS - 1)] {
        assert_eq!(
            parse_key(&segment_key(u64::MAX, token(), segment, part)).unwrap(),
            ColumnKey::Segment {
                group: u64::MAX,
                token: token(),
                segment,
                part,
            }
        );
    }
    for slot in [0, 1, SLOTS - 1] {
        assert_eq!(
            parse_key(&delta_key(GROUP, slot)).unwrap(),
            ColumnKey::Delta { group: GROUP, slot }
        );
    }
    let mut beyond = segment_key(GROUP, token(), 0, 0).to_vec();
    let at = beyond.len() - 2;
    beyond[at] = SEGMENTS as u8;
    assert!(parse_key(&beyond).is_err_and(|error| is_corruption(&error)));
    let mut past = segment_key(GROUP, token(), 0, 0).to_vec();
    let at = past.len() - 1;
    past[at] = SEGMENT_SLOTS as u8;
    assert!(parse_key(&past).is_err_and(|error| is_corruption(&error)));
    let mut outside = delta_key(GROUP, 0);
    outside[9..].copy_from_slice(&(SLOTS as u16).to_be_bytes());
    assert!(parse_key(&outside).is_err_and(|error| is_corruption(&error)));
    let mut kind = delta_key(GROUP, 0);
    kind[0] = 2;
    assert!(parse_key(&kind).is_err_and(|error| is_corruption(&error)));
    for length in [0, 1, 10, 12, 13, 14, 16] {
        let mut key = vec![0; length];
        if let Some(byte) = key.first_mut() {
            *byte = DELTA_KIND;
        }
        assert!(parse_key(&key).is_err_and(|error| is_corruption(&error)));
    }
    assert!(segment_key(GROUP, token(), SEGMENTS - 1, 0) < segment_key(GROUP, Token::new(8), 0, 0));
    assert!(segment_key(GROUP, Token::new(u32::MAX), SEGMENTS - 1, 0) < segments_end(GROUP));
    assert!(segments_end(GROUP) < segment_key(GROUP + 1, Token::new(0), 0, 0));
    assert!(
        delta_key(u64::MAX, SLOTS - 1)[..] < segment_key(0, Token::new(0), 0, 0)[..],
        "every delta sorts before every segment, so a load appends segments at the end"
    );
    assert!(segment_key(GROUP, token(), 2, SEGMENT_SLOTS - 1) < segment_key(GROUP, token(), 3, 0));
    assert_eq!(
        &segment_key(GROUP, token(), 2, 9)[..segment_key(GROUP, token(), 2, 9).len() - 1],
        &parts_prefix(GROUP, token(), 2)[..]
    );
    assert!(delta_key(GROUP, SLOTS - 1) < deltas_range(GROUP).1);
    assert!(deltas_range(GROUP).1 <= delta_key(GROUP + 1, 0));
    assert_eq!(deltas_range(GROUP).0, delta_key(GROUP, 0));
    assert_eq!(
        &segment_key(GROUP, token(), 9, 0)[..column_prefix(GROUP, token()).len()],
        &column_prefix(GROUP, token())[..]
    );
    assert_eq!(&delta_key(GROUP, 9)[..9], &deltas_prefix(GROUP));
}

fn integer(value: i64) -> Option<PropertyValue> {
    Some(PropertyValue::Integer(value))
}

#[test]
fn an_edit_reads_its_own_values_and_leaves_the_rest_to_storage_unless_it_created_the_node() {
    let mut edit = ColumnsEdit::load(2);
    let current =
        |edit: &ColumnsEdit, slot: usize, key: Token<PropertyKey>| edit.current(slot, key).unwrap();
    assert_eq!(current(&edit, 0, token()), None);
    edit.set(0, token(), integer(5));
    assert_eq!(current(&edit, 0, token()), Some(integer(5)));
    assert_eq!(
        current(&edit, 0, Token::new(1)),
        None,
        "another key reads as stored"
    );
    edit.set(0, token(), None);
    assert_eq!(current(&edit, 0, token()), Some(None));
    edit.create(4, Vec::new()).unwrap();
    assert_eq!(
        current(&edit, 4, token()),
        Some(None),
        "a new node holds nothing stored"
    );
    edit.set(4, token(), integer(1));
    edit.create(4, Vec::new()).unwrap();
    assert_eq!(current(&edit, 4, token()), Some(integer(1)));
    let list = PropertyValue::List(vec![
        PropertyValue::Float(0.5),
        PropertyValue::Boolean(true),
    ]);
    edit.create(
        6,
        vec![
            (Token::new(1), PropertyValue::String("six".to_owned())),
            (Token::new(3), list.clone()),
        ],
    )
    .unwrap();
    assert_eq!(
        current(&edit, 6, Token::new(3)),
        Some(Some(list.clone())),
        "a created node reads the values it was created with"
    );
    assert_eq!(current(&edit, 6, Token::new(2)), Some(None));
    edit.set(6, Token::new(1), None);
    edit.set(6, Token::new(2), integer(7));
    assert_eq!(current(&edit, 6, Token::new(1)), Some(None));
    assert_eq!(current(&edit, 6, Token::new(2)), Some(integer(7)));
    assert_eq!(current(&edit, 6, Token::new(3)), Some(Some(list.clone())));
    edit.create(8, vec![(Token::new(1), PropertyValue::Integer(8))])
        .unwrap();
    edit.set(8, Token::new(1), None);
    edit.create(8, vec![(Token::new(2), PropertyValue::Integer(9))])
        .unwrap();
    assert_eq!(
        current(&edit, 8, Token::new(1)),
        Some(None),
        "a node created again in its slot keeps what the transaction removed"
    );
    assert_eq!(current(&edit, 8, Token::new(2)), Some(integer(9)));
    assert_eq!(edit.tokens(0).unwrap(), vec![token()]);
    assert_eq!(edit.tokens(2).unwrap(), vec![]);
    assert_eq!(
        edit.tokens(6).unwrap(),
        vec![Token::new(1), Token::new(2), Token::new(3)]
    );
    let (deltas, slots, heap) = edit.into_slots();
    assert_eq!(deltas, 2);
    assert_eq!(
        slots.iter().map(|(slot, _)| *slot).collect::<Vec<_>>(),
        vec![0, 4, 6, 8]
    );
    assert!(slots[1].1.fresh);
    assert!(!slots[0].1.fresh);
    let six = slots[2].1.clone();
    assert!(six.created(&heap).is_some());
    assert_eq!(
        six.into_changes(&heap).unwrap(),
        vec![
            (Token::new(1), None),
            (Token::new(2), integer(7)),
            (Token::new(3), Some(list)),
        ]
    );
}

#[test]
fn a_node_created_in_a_transaction_stores_the_delta_its_values_encode_to() {
    let values = vec![
        (Token::new(1), PropertyValue::String("one".to_owned())),
        (Token::new(2), PropertyValue::Integer(-2)),
        (
            Token::new(5),
            PropertyValue::List(vec![PropertyValue::Float(1.5); 3]),
        ),
    ];
    let mut edit = ColumnsEdit::load(0);
    edit.create(9, values.clone()).unwrap();
    let (_, slots, heap) = edit.into_slots();
    let created = slots[0].1.created(&heap).unwrap();
    let key = delta_key(GROUP, 9);
    let expected = encode_delta(
        &key,
        values
            .iter()
            .map(|(token, value)| (*token, Some(Cell::of(value)))),
    );
    assert_eq!(encode_created_delta(&key, created), expected);
    assert!(DeltaRef::parse(&key, &expected).is_ok());
}

#[test]
fn a_reader_keeps_the_property_keys_of_each_group_it_listed() {
    let mut reader = ColumnReader::default();
    assert_eq!(reader.keys(5), None);
    reader.insert_keys(5, vec![Token::new(1), Token::new(4)]);
    reader.insert_keys(2, Vec::new());
    reader.insert_keys(5, vec![Token::new(9)]);
    assert_eq!(reader.keys(5), Some(&[Token::new(1), Token::new(4)][..]));
    assert_eq!(reader.keys(2), Some(&[][..]));
    assert_eq!(reader.keys(3), None);
}

#[test]
fn loaded_deltas_are_found_by_slot() {
    let deltas: Deltas = [3, 9, 40]
        .into_iter()
        .map(|slot| {
            let key = delta_key(GROUP, slot);
            let bytes = encode_entries(&key, &[(token(), integer(slot as i64))]);
            (slot, Delta::load(&key, &bytes).unwrap())
        })
        .collect();
    for slot in [3, 9, 40] {
        let found = find_delta(&deltas, slot).unwrap();
        let value = found.view().get(token()).unwrap().unwrap().unwrap();
        assert_eq!(
            value.to_value().unwrap(),
            PropertyValue::Integer(slot as i64)
        );
    }
    for slot in [0, 4, 41] {
        assert!(find_delta(&deltas, slot).is_none());
    }
}
