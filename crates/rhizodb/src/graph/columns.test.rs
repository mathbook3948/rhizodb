use proptest::prelude::*;
use proptest::test_runner::TestCaseError;

use super::*;
use crate::value::tests::property_value;
use crate::value::tests::same_bits;

const GROUP: u64 = 3;

fn token() -> Token<PropertyKey> {
    Token::new(7)
}

fn is_corruption(error: &GraphError) -> bool {
    matches!(error, GraphError::Corrupt(_) | GraphError::Decode(_))
}

fn encode(values: &[(usize, PropertyValue)]) -> Option<Vec<u8>> {
    let cells: Vec<(usize, Cell<'_>)> = values
        .iter()
        .map(|(slot, value)| (*slot, Cell::of(value)))
        .collect();
    encode_chunk(GROUP, token(), &cells).unwrap()
}

fn kind_of(chunk: &[u8]) -> Kind {
    Kind::from_tag(chunk[KIND_AT]).unwrap()
}

fn read_back(chunk: &[u8]) -> Result<Vec<(usize, PropertyValue)>, GraphError> {
    let view = ChunkRef::parse(GROUP, token(), chunk)?;
    view.verify()?;
    view.cells()
        .map(|cell| {
            let (slot, cell) = cell?;
            Ok((slot, cell.to_value()?))
        })
        .collect()
}

fn same_values(left: &[(usize, PropertyValue)], right: &[(usize, PropertyValue)]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|((a, x), (b, y))| a == b && same_bits(x, y))
}

fn assert_reads_back(values: &[(usize, PropertyValue)]) -> Result<Vec<u8>, TestCaseError> {
    let chunk = encode(values).unwrap();
    let read = read_back(&chunk).unwrap();
    prop_assert!(same_values(&read, values), "{:?} != {:?}", read, values);
    let view = ChunkRef::parse(GROUP, token(), &chunk).unwrap();
    let column = Column::load(GROUP, token(), &chunk).unwrap();
    let head = view.head();
    let mut expected = values.iter().peekable();
    for slot in 0..SLOTS {
        let want = expected
            .next_if(|(held, _)| *held == slot)
            .map(|(_, value)| value);
        let point = view
            .cell_at(slot)
            .unwrap()
            .map(|cell| cell.to_value().unwrap());
        let loaded = column
            .cell_at(slot)
            .unwrap()
            .map(|cell| cell.to_value().unwrap());
        prop_assert_eq!(head.has(slot), want.is_some(), "presence of {}", slot);
        for found in [point, loaded] {
            match (&found, want) {
                (Some(found), Some(want)) => prop_assert!(same_bits(found, want), "{}", slot),
                (None, None) => {}
                _ => prop_assert!(false, "slot {}: {:?} != {:?}", slot, found, want),
            }
        }
    }
    prop_assert_eq!(head.count, values.len());
    let again = encode(&read);
    prop_assert_eq!(
        again.as_deref(),
        Some(&chunk[..]),
        "the encoding is canonical"
    );
    Ok(chunk)
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

fn column_values(kind: u8, most: usize) -> impl Strategy<Value = Vec<(usize, PropertyValue)>> {
    let value = if kind < 4 {
        typed_value(kind)
    } else {
        property_value().boxed()
    };
    prop::collection::btree_set(0..SLOTS, 1..most).prop_flat_map(move |slots| {
        let count = slots.len();
        let slots: Vec<usize> = slots.into_iter().collect();
        prop::collection::vec(value.clone(), count)
            .prop_map(move |values| slots.iter().copied().zip(values).collect::<Vec<_>>())
    })
}

fn full(
    value: impl Fn(usize) -> PropertyValue,
    slots: impl Iterator<Item = usize>,
) -> Vec<(usize, PropertyValue)> {
    slots.map(|slot| (slot, value(slot))).collect()
}

proptest! {
    #[test]
    fn a_column_of_one_type_reads_back_every_value_from_every_slot(
        values in (0u8..4).prop_flat_map(|kind| column_values(kind, 300)),
    ) {
        let chunk = assert_reads_back(&values)?;
        let expected = match values[0].1 {
            PropertyValue::Boolean(_) => Kind::Bool,
            PropertyValue::Integer(_) => Kind::Int,
            PropertyValue::Float(_) => Kind::Float,
            _ => Kind::Str,
        };
        prop_assert_eq!(kind_of(&chunk), expected);
    }

    #[test]
    fn a_column_of_mixed_types_reads_back_every_value_from_every_slot(
        values in column_values(4, 200),
    ) {
        let chunk = assert_reads_back(&values)?;
        let kinds: BTreeSet<u8> = values
            .iter()
            .map(|(_, value)| Cell::of(value).kind().tag())
            .collect();
        let expected = if kinds.len() == 1 {
            Cell::of(&values[0].1).kind()
        } else {
            Kind::Variant
        };
        prop_assert_eq!(kind_of(&chunk), expected);
    }

    #[test]
    fn a_delta_reads_back_its_slot_and_value(
        slot in 0..SLOTS,
        value in prop::option::of(property_value()),
    ) {
        let bytes = encode_delta(GROUP, token(), slot, value.as_ref().map(Cell::of));
        let (read_slot, read) = parse_delta(&delta_key(GROUP, token(), slot), &bytes).unwrap();
        prop_assert_eq!(read_slot, slot);
        match (&read, &value) {
            (Some(read), Some(value)) => prop_assert!(same_bits(read, value)),
            (None, None) => {}
            _ => prop_assert!(false, "{:?} != {:?}", read, value),
        }
    }

    #[test]
    fn merged_values_hold_each_change_over_the_stored_value(
        stored in column_values(1, 100),
        changes in prop::collection::btree_map(0..SLOTS, prop::option::of(any::<i64>()), 0..100),
    ) {
        let chunk = encode(&stored).unwrap();
        let view = ChunkRef::parse(GROUP, token(), &chunk).unwrap();
        let changes: BTreeMap<usize, Option<PropertyValue>> = changes
            .into_iter()
            .map(|(slot, value)| (slot, value.map(PropertyValue::Integer)))
            .collect();
        let cells = merged(view.cells(), &changes).unwrap();
        let mut expected: BTreeMap<usize, PropertyValue> = stored.iter().cloned().collect();
        for (slot, value) in &changes {
            match value {
                Some(value) => expected.insert(*slot, value.clone()),
                None => expected.remove(slot),
            };
        }
        let found: Vec<(usize, PropertyValue)> = cells
            .iter()
            .map(|(slot, cell)| (*slot, cell.to_value().unwrap()))
            .collect();
        let expected: Vec<(usize, PropertyValue)> = expected.into_iter().collect();
        prop_assert!(same_values(&found, &expected));
    }
}

#[test]
fn columns_of_every_size_at_block_and_group_edges_read_back() {
    let makers: [fn(usize) -> PropertyValue; 5] = [
        |slot| PropertyValue::Boolean(slot % 3 == 0),
        |slot| PropertyValue::Integer(slot as i64 * 7 - 9000),
        |slot| PropertyValue::Float(slot as f64 / 3.0),
        |slot| PropertyValue::String("s".repeat(slot % 5)),
        |slot| {
            if slot % 2 == 0 {
                PropertyValue::Integer(slot as i64)
            } else {
                PropertyValue::List(vec![PropertyValue::String(slot.to_string())])
            }
        },
    ];
    for make in makers {
        for count in [1, 2, 7, 8, 9, 63, 64, 65, 127, 128, 129, 4095, 4096] {
            assert_reads_back(&full(make, 0..count)).unwrap();
            assert_reads_back(&full(make, SLOTS - count..SLOTS)).unwrap();
        }
        assert_reads_back(&full(make, (0..SLOTS).step_by(64))).unwrap();
        assert_reads_back(&full(make, (63..SLOTS).step_by(64))).unwrap();
        assert_reads_back(&full(make, [0, SLOTS - 1].into_iter())).unwrap();
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
    let chunk = assert_reads_back(&values).unwrap();
    assert_eq!(kind_of(&chunk), Kind::Int);
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
    let chunk = assert_reads_back(&values).unwrap();
    assert_eq!(kind_of(&chunk), Kind::Float);
    let long = "long".repeat(5000);
    let strings = ["", "a", "ünïcödé ✓", "\0", long.as_str()];
    let values: Vec<(usize, PropertyValue)> = strings
        .iter()
        .map(|text| PropertyValue::String((*text).to_owned()))
        .enumerate()
        .collect();
    let chunk = assert_reads_back(&values).unwrap();
    assert_eq!(kind_of(&chunk), Kind::Str);
    let only_empty = [(4095, PropertyValue::String(String::new()))];
    let chunk = assert_reads_back(&only_empty).unwrap();
    assert_eq!(chunk.len(), BLOCKS_AT + 4 + 4, "one end offset and no heap");
}

#[test]
fn a_column_without_values_has_no_chunk() {
    assert_eq!(encode(&[]), None);
}

#[test]
fn values_in_their_tagged_encoding_store_as_the_typed_chunk_they_would_be() {
    let values = [
        PropertyValue::Integer(5),
        PropertyValue::String("x".to_owned()),
        PropertyValue::Boolean(true),
        PropertyValue::Float(-0.0),
    ];
    for value in values {
        let mut tagged = Vec::new();
        value.encode_into(&mut tagged);
        let encoded = encode_chunk(GROUP, token(), &[(9, Cell::Encoded(&tagged))])
            .unwrap()
            .unwrap();
        let typed = encode(&[(9, value.clone())]).unwrap();
        assert_eq!(kind_of(&encoded), Kind::Variant);
        assert_ne!(kind_of(&typed), Kind::Variant);
        let read = read_back(&encoded).unwrap();
        assert!(same_values(&read, &[(9, value)]));
        assert_eq!(
            encode(&read).unwrap(),
            typed,
            "reading it back gives the typed chunk"
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

fn sample_chunks() -> Vec<(&'static str, Vec<(usize, PropertyValue)>)> {
    vec![
        (
            "bool",
            full(
                |slot| PropertyValue::Boolean(slot % 2 == 1),
                [1, 5, 9, 4095].into_iter(),
            ),
        ),
        (
            "int",
            full(|slot| PropertyValue::Integer(slot as i64 - 3), 0..70),
        ),
        (
            "float",
            full(|slot| PropertyValue::Float(slot as f64 * 0.5), 10..80),
        ),
        (
            "string",
            full(|slot| PropertyValue::String(format!("v{slot}")), 0..130),
        ),
        (
            "variant",
            full(
                |slot| match slot % 3 {
                    0 => PropertyValue::Integer(slot as i64),
                    1 => PropertyValue::String(format!("s{slot}")),
                    _ => PropertyValue::List(vec![PropertyValue::Boolean(true)]),
                },
                0..70,
            ),
        ),
        (
            "full ints",
            full(|slot| PropertyValue::Integer(slot as i64), 0..SLOTS),
        ),
    ]
}

fn positions(length: usize) -> Vec<usize> {
    let head = length.min(BLOCKS_AT + 64);
    let stride = (length / 600).max(1);
    let mut positions: Vec<usize> = (0..head).collect();
    positions.extend((head..length).step_by(stride));
    positions.push(length - 1);
    positions
}

#[test]
fn a_changed_byte_anywhere_in_a_chunk_fails_its_check_and_never_misreads_a_value() {
    for (name, values) in sample_chunks() {
        let chunk = encode(&values).unwrap();
        for at in positions(chunk.len()) {
            for flip in [0x01u8, 0x80, 0xff] {
                let mut changed = chunk.clone();
                changed[at] ^= flip;
                let whole = read_back(&changed);
                assert!(
                    whole.as_ref().is_err_and(is_corruption),
                    "{name}: byte {at} ^ {flip:#x} read as {whole:?}"
                );
                assert!(
                    Column::load(GROUP, token(), &changed)
                        .is_err_and(|error| is_corruption(&error)),
                    "{name}: byte {at} ^ {flip:#x} loads"
                );
                let Ok(view) = ChunkRef::parse(GROUP, token(), &changed) else {
                    continue;
                };
                for (slot, value) in values.iter().step_by(values.len() / 256 + 1) {
                    match view.cell_at(*slot) {
                        Ok(Some(cell)) => {
                            let read = cell.to_value();
                            assert!(
                                read.as_ref().is_ok_and(|read| same_bits(read, value))
                                    || read.as_ref().is_err_and(is_corruption),
                                "{name}: byte {at} ^ {flip:#x} misreads slot {slot}: {read:?}"
                            );
                        }
                        Ok(None) => panic!("{name}: byte {at} ^ {flip:#x} loses slot {slot}"),
                        Err(error) => assert!(is_corruption(&error)),
                    }
                }
            }
        }
    }
}

#[test]
fn a_cut_padded_or_emptied_chunk_fails_its_check() {
    for (name, values) in sample_chunks() {
        let chunk = encode(&values).unwrap();
        let mut padded = chunk.clone();
        padded.push(0);
        let changes = [
            chunk[..chunk.len() - 1].to_vec(),
            chunk[..chunk.len() / 2].to_vec(),
            chunk[..BLOCKS_AT].to_vec(),
            chunk[..1].to_vec(),
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
fn a_chunk_read_under_the_key_of_another_column_fails_its_check() {
    let chunk = encode(&[(0, PropertyValue::Integer(1))]).unwrap();
    for (group, token) in [
        (GROUP + 1, token()),
        (GROUP, Token::new(8)),
        (0, Token::new(0)),
    ] {
        let error = ChunkRef::parse(group, token, &chunk).unwrap_err();
        assert!(is_corruption(&error));
    }
}

fn reseal(chunk: &mut [u8]) {
    let layout = Layout::of(chunk).unwrap();
    for block in 0..layout.count.div_ceil(BLOCK) {
        if let Ok(sum) = layout.block_checksum(chunk, block) {
            let at = BLOCKS_AT + 4 * block;
            chunk[at..at + 4].copy_from_slice(&sum.to_le_bytes());
        }
    }
    let head = crc(&[&chunk[KIND_AT..layout.head_end]]);
    chunk[HEAD_CHECKSUM..KIND_AT].copy_from_slice(&head.to_le_bytes());
}

#[test]
fn a_chunk_of_an_unknown_layout_type_or_count_fails_its_check() {
    let chunk = encode(&full(|slot| PropertyValue::Integer(slot as i64), 0..3)).unwrap();
    let mut layout = chunk.clone();
    layout[0] = LAYOUT_VERSION + 1;
    let mut kind = chunk.clone();
    kind[KIND_AT] = 9;
    let mut count = chunk.clone();
    count[COUNT_AT..COUNT_AT + 2].copy_from_slice(&0u16.to_be_bytes());
    let mut heap = chunk.clone();
    heap[HEAP_AT + 3] = 8;
    for (name, changed) in [
        ("layout", layout),
        ("kind", kind),
        ("count", count),
        ("heap", heap),
    ] {
        let result = read_back(&changed);
        assert!(
            result.as_ref().is_err_and(is_corruption),
            "{name}: {result:?}"
        );
    }
}

#[test]
fn a_resealed_chunk_whose_parts_disagree_still_fails_as_corruption() {
    let values = full(|slot| PropertyValue::String(format!("v{slot}")), 0..70);
    let chunk = encode(&values).unwrap();
    let layout = Layout::of(&chunk).unwrap();
    let mut count = chunk.clone();
    count[BITMAP_AT + 8 * 10] ^= 0x01;
    let mut descending = chunk.clone();
    let at = layout.values + 4 * 3;
    descending[at..at + 4].copy_from_slice(&1u32.to_le_bytes());
    let mut outside = chunk.clone();
    let last = layout.values + 4 * 69;
    outside[last..last + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    let mut not_utf8 = chunk.clone();
    not_utf8[layout.heap] = 0xff;
    for (name, mut changed) in [
        ("count", count),
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
fn a_resealed_variant_chunk_with_an_undecodable_value_fails_where_it_is_read() {
    let values = full(
        |slot| PropertyValue::List(vec![PropertyValue::Integer(slot as i64)]),
        0..4,
    );
    let mut chunk = encode(&values).unwrap();
    let layout = Layout::of(&chunk).unwrap();
    chunk[layout.heap] = 0xee;
    reseal(&mut chunk);
    assert!(read_back(&chunk).is_err_and(|error| is_corruption(&error)));
    let view = ChunkRef::parse(GROUP, token(), &chunk).unwrap();
    assert!(view.cell_at(0).is_err_and(|error| is_corruption(&error)));
    assert!(view.cell_at(1).unwrap().unwrap().to_value().is_ok());
}

#[test]
fn a_changed_or_cut_delta_or_one_under_another_key_fails_its_check() {
    let values = [
        None,
        Some(PropertyValue::Integer(-4)),
        Some(PropertyValue::String("text".to_owned())),
        Some(PropertyValue::List(vec![PropertyValue::Float(0.5)])),
    ];
    for value in &values {
        let key = delta_key(GROUP, token(), 17);
        let bytes = encode_delta(GROUP, token(), 17, value.as_ref().map(Cell::of));
        for at in 0..bytes.len() {
            for flip in [0x01u8, 0x80, 0xff] {
                let mut changed = bytes.clone();
                changed[at] ^= flip;
                let result = parse_delta(&key, &changed);
                assert!(
                    result.as_ref().is_err_and(is_corruption),
                    "{value:?} byte {at}"
                );
            }
        }
        for cut in 0..bytes.len() {
            assert!(parse_delta(&key, &bytes[..cut]).is_err_and(|error| is_corruption(&error)));
        }
        let mut padded = bytes.clone();
        padded.push(0);
        assert!(parse_delta(&key, &padded).is_err_and(|error| is_corruption(&error)));
        for other in [
            delta_key(GROUP, token(), 18),
            delta_key(GROUP, Token::new(8), 17),
            delta_key(GROUP + 1, token(), 17),
        ] {
            assert!(parse_delta(&other, &bytes).is_err_and(|error| is_corruption(&error)));
        }
        assert!(
            parse_delta(&chunk_key(GROUP, token()), &bytes)
                .is_err_and(|error| is_corruption(&error))
        );
    }
}

#[test]
fn a_resealed_delta_with_an_unknown_state_or_trailing_bytes_fails_as_corruption() {
    let key = delta_key(GROUP, token(), 2);
    let seal = |body: &[u8]| {
        let mut bytes = body.to_vec();
        bytes.extend_from_slice(&crc(&[&key, body]).to_le_bytes());
        bytes
    };
    let mut tagged = vec![PRESENT];
    PropertyValue::Integer(3).encode_into(&mut tagged);
    let mut trailing = tagged.clone();
    trailing.push(0);
    for body in [vec![2], vec![ABSENT, 0], vec![PRESENT], trailing] {
        let result = parse_delta(&key, &seal(&body));
        assert!(
            result.as_ref().is_err_and(is_corruption),
            "{body:?}: {result:?}"
        );
    }
    assert_eq!(
        parse_delta(&key, &seal(&tagged)).unwrap().1,
        Some(PropertyValue::Integer(3))
    );
}

#[test]
fn keys_name_their_group_column_and_part_and_others_fail() {
    assert_eq!(
        parse_key(&chunk_key(GROUP, token())).unwrap(),
        (GROUP, token(), Part::Chunk)
    );
    for slot in [0, 1, SLOTS - 1] {
        assert_eq!(
            parse_key(&delta_key(u64::MAX, token(), slot)).unwrap(),
            (u64::MAX, token(), Part::Delta(slot))
        );
    }
    let mut beyond = chunk_key(GROUP, token());
    beyond[12..].copy_from_slice(&(SLOTS as u32 + 1).to_be_bytes());
    assert!(parse_key(&beyond).is_err_and(|error| is_corruption(&error)));
    for length in [0, 15, 17] {
        assert!(parse_key(&vec![0; length]).is_err_and(|error| is_corruption(&error)));
    }
    assert!(chunk_key(GROUP, token()) < delta_key(GROUP, token(), 0));
    assert!(delta_key(GROUP, token(), SLOTS - 1) < chunk_key(GROUP, Token::new(8)));
    assert!(
        delta_key(GROUP, Token::new(u32::MAX), SLOTS - 1) < chunk_key(GROUP + 1, Token::new(0))
    );
    assert_eq!(
        &chunk_key(GROUP, token())[..12],
        &column_prefix(GROUP, token())
    );
    assert_eq!(&chunk_key(GROUP, token())[..8], &group_prefix(GROUP));
}

fn stored_head(slots: impl Iterator<Item = usize>) -> StoredHead {
    let values = full(|slot| PropertyValue::Integer(slot as i64), slots);
    let chunk = encode(&values).unwrap();
    ChunkRef::parse(GROUP, token(), &chunk).unwrap().head()
}

fn integer(value: i64) -> Option<PropertyValue> {
    Some(PropertyValue::Integer(value))
}

#[test]
fn an_edit_writes_deltas_until_the_column_would_keep_more_than_the_limit() {
    for stored in [0, 1, MAX_DELTAS - 1, MAX_DELTAS] {
        let deltas: Vec<(usize, Option<PropertyValue>)> =
            (0..stored).map(|slot| (1000 + slot, integer(1))).collect();
        for fresh in [1, MAX_DELTAS - stored, MAX_DELTAS - stored + 1] {
            let mut edit = ColumnEdit::load(Some(stored_head(0..200)), deltas.clone()).unwrap();
            for slot in 0..fresh {
                edit.set(slot, integer(-1));
                edit.set(slot, integer(-2));
            }
            if stored > 0 {
                edit.set(1000, None);
            }
            let rewrite = stored + fresh > MAX_DELTAS;
            match edit.flush() {
                ColumnFlush::Rewrite { values, stale } => {
                    assert!(rewrite, "{stored} stored and {fresh} fresh rewrite");
                    assert_eq!(stale.len(), stored);
                    assert_eq!(values.len(), stored + fresh);
                }
                ColumnFlush::Deltas {
                    touched,
                    stored: held,
                } => {
                    assert!(!rewrite, "{stored} stored and {fresh} fresh keep deltas");
                    assert_eq!(held.len(), stored);
                    assert_eq!(touched.len(), fresh + usize::from(stored > 0));
                    assert!(touched.iter().all(|(slot, had, _)| *had == (*slot < 200)));
                }
            }
        }
    }
}

#[test]
fn an_edit_of_a_column_without_a_chunk_or_left_empty_rewrites_it() {
    let mut fresh = ColumnEdit::load(None, Vec::new()).unwrap();
    fresh.set(4, integer(1));
    assert_eq!(fresh.current(4), Some(Some(&PropertyValue::Integer(1))));
    assert_eq!(fresh.current(5), Some(None));
    assert!(matches!(fresh.flush(), ColumnFlush::Rewrite { .. }));
    let mut emptied = ColumnEdit::load(Some(stored_head(0..2)), vec![(5, integer(9))]).unwrap();
    for slot in [0, 1, 5] {
        emptied.set(slot, None);
    }
    match emptied.flush() {
        ColumnFlush::Rewrite { values, stale } => {
            assert!(values.values().all(Option::is_none));
            assert_eq!(stale, [5].into());
        }
        other => panic!("an emptied column keeps {other:?}"),
    }
    let error = ColumnEdit::load(None, vec![(1, integer(1))]).unwrap_err();
    assert!(is_corruption(&error));
}

#[test]
fn an_edit_reads_its_own_values_then_the_deltas_then_the_chunk() {
    let mut edit =
        ColumnEdit::load(Some(stored_head(0..3)), vec![(1, None), (7, integer(70))]).unwrap();
    assert_eq!(edit.current(0), None, "the chunk holds slot 0");
    assert_eq!(edit.current(1), Some(None));
    assert_eq!(edit.current(7), Some(Some(&PropertyValue::Integer(70))));
    assert_eq!(edit.current(8), Some(None));
    edit.set(0, None);
    edit.set(8, integer(80));
    assert_eq!(edit.current(0), Some(None));
    assert_eq!(edit.current(8), Some(Some(&PropertyValue::Integer(80))));
}

#[test]
fn a_reader_probes_a_column_once_then_loads_it() {
    let mut reader = ColumnReader::default();
    let id = (GROUP, token());
    assert!(matches!(reader.read(id, 0).unwrap(), ColumnRead::Probe));
    let chunk = encode(&full(|slot| PropertyValue::Integer(slot as i64), 0..10)).unwrap();
    let column = Column::load(GROUP, token(), &chunk).unwrap();
    match reader.read(id, 3).unwrap() {
        ColumnRead::Load(cached) => {
            let value = cached
                .fill(Some(column), vec![(4, None), (20, integer(5))], 3)
                .unwrap();
            assert_eq!(value, integer(3));
        }
        _ => panic!("a column read twice is loaded"),
    }
    for (slot, expected) in [(3, integer(3)), (4, None), (20, integer(5)), (30, None)] {
        match reader.read(id, slot).unwrap() {
            ColumnRead::Value(value) => assert_eq!(value, expected, "slot {slot}"),
            _ => panic!("a loaded column reads from memory"),
        }
    }
    let other = (GROUP, Token::new(1));
    assert!(matches!(reader.read(other, 0).unwrap(), ColumnRead::Probe));
    match reader.read(other, 0).unwrap() {
        ColumnRead::Load(cached) => {
            assert_eq!(cached.fill(None, Vec::new(), 0).unwrap(), None);
        }
        _ => panic!("a column read twice is loaded"),
    }
    assert!(matches!(
        reader.read(other, 9).unwrap(),
        ColumnRead::Value(None)
    ));
    match reader.read(id, 0).unwrap() {
        ColumnRead::Value(value) => assert_eq!(value, integer(0)),
        _ => panic!("the first column stays loaded"),
    }
    let third = (GROUP + 1, token());
    assert!(matches!(reader.read(third, 0).unwrap(), ColumnRead::Probe));
    match reader.read(third, 0).unwrap() {
        ColumnRead::Load(cached) => {
            assert!(
                cached
                    .fill(None, vec![(0, None)], 0)
                    .is_err_and(|error| is_corruption(&error))
            );
        }
        _ => panic!("a column read twice is loaded"),
    }
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
