use std::collections::BTreeMap;

use super::*;
use crate::graph::ids::NodeId;
use crate::graph::ids::Token;
use crate::graph::records::Properties;
use crate::value::tests::property_value;
use proptest::prelude::*;

const KEY: [u8; 8] = 7u64.to_be_bytes();

fn number() -> impl Strategy<Value = u64> {
    prop_oneof![0u64..40, 1u64 << 40..(1u64 << 40) + 300, any::<u64>()]
}

fn value() -> impl Strategy<Value = PropertyValue> {
    prop_oneof![
        3 => prop_oneof![-5i64..5, 1990i64..2025, Just(i64::MIN), Just(i64::MAX), any::<i64>()]
            .prop_map(PropertyValue::Integer),
        1 => property_value(),
    ]
}

fn record() -> impl Strategy<Value = Vec<u8>> {
    (
        prop_oneof![0u32..3, Just(u32::MAX)],
        number(),
        number(),
        prop::collection::btree_map((0u32..6).prop_map(Token::new), value(), 0..4),
    )
        .prop_map(|(rel_type, source, target, properties)| {
            RelationshipRecord {
                rel_type: Token::new(rel_type),
                source: NodeId(source),
                target: NodeId(target),
                properties: Properties::from_iter(properties),
            }
            .encode()
        })
}

fn edits() -> impl Strategy<Value = Vec<(usize, Option<Vec<u8>>)>> {
    prop::collection::vec(
        (0..CHUNK_SLOTS, prop::option::weighted(0.8, record())),
        0..120,
    )
}

fn edited(edits: Vec<(usize, Option<Vec<u8>>)>) -> (ChunkEdit, BTreeMap<usize, Vec<u8>>) {
    let mut edit = ChunkEdit::default();
    let mut model = BTreeMap::new();
    for (slot, record) in edits {
        match &record {
            Some(record) => model.insert(slot, record.clone()),
            None => model.remove(&slot),
        };
        edit.set(slot, record.as_deref());
    }
    (edit, model)
}

fn records_of(model: &BTreeMap<usize, Vec<u8>>) -> Vec<(usize, Vec<u8>)> {
    model.clone().into_iter().collect()
}

proptest! {
    #[test]
    #[ignore = "left out of mise run check, run by mise run random"]
    fn a_chunk_reads_back_what_its_edits_left(edits in edits()) {
        let (edit, model) = edited(edits);
        prop_assert_eq!(edit.bytes(), model.values().map(Vec::len).sum::<usize>());
        let Some(stored) = edit.encode(&KEY).unwrap() else {
            prop_assert!(model.is_empty());
            return Ok(());
        };
        let chunk = ChunkRef::parse(&KEY, &stored).unwrap();
        for slot in 0..CHUNK_SLOTS {
            prop_assert_eq!(chunk.get(slot).unwrap(), model.get(&slot).cloned());
        }
        for (slot, record) in &model {
            let decoded = RelationshipRecord::decode(record).unwrap();
            prop_assert_eq!(chunk.rel_type(*slot).unwrap(), Some(decoded.rel_type.id()));
            prop_assert_eq!(
                chunk.keys(*slot).unwrap(),
                Some(decoded.properties.keys().map(|key| key.id()).collect())
            );
            for key in 0..7 {
                let read = chunk.property(*slot, key).unwrap().unwrap();
                let expected = RelationshipRecord::property(record, Token::new(key)).unwrap();
                prop_assert_eq!(format!("{read:?}"), format!("{expected:?}"));
            }
        }
        let entries: Vec<(usize, Vec<u8>)> = chunk.entries().collect::<Result<_, _>>().unwrap();
        prop_assert_eq!(entries, records_of(&model));
        let loaded = ChunkEdit::load(&KEY, &stored).unwrap();
        prop_assert_eq!(loaded.encode(&KEY).unwrap(), Some(stored.clone()));
        let older = [
            in_records_layout(&KEY, &records_of(&model)),
            unsealed(&KEY, &records_of(&model)),
        ];
        for older in older {
            prop_assert_eq!(upgrade(&KEY, &older).unwrap(), Some(stored.clone()));
        }
        prop_assert_eq!(upgrade(&KEY, &stored).unwrap(), None);
    }

    #[test]
    #[ignore = "left out of mise run check, run by mise run random"]
    fn a_damaged_chunk_fails_to_read(
        edits in edits(),
        cut in any::<prop::sample::Index>(),
        flip in any::<prop::sample::Index>(),
    ) {
        let (edit, _) = edited(edits);
        let Some(mut stored) = edit.encode(&KEY).unwrap() else {
            return Ok(());
        };
        let at = flip.index(stored.len());
        stored[at] ^= 0x5a;
        stored.truncate(cut.index(stored.len() + 1));
        prop_assert!(ChunkRef::parse(&KEY, &stored).is_err());
        prop_assert!(ChunkEdit::load(&KEY, &stored).is_err());
        prop_assert!(upgrade(&KEY, &stored).is_err());
    }

    #[test]
    #[ignore = "left out of mise run check, run by mise run random"]
    fn a_damaged_chunk_of_an_older_layout_fails_to_upgrade_or_upgrades_its_framed_records(
        edits in edits(),
        sealed in any::<bool>(),
        cut in any::<prop::sample::Index>(),
        flip in any::<prop::sample::Index>(),
    ) {
        let (_, model) = edited(edits);
        if model.is_empty() {
            return Ok(());
        }
        let mut old = if sealed {
            in_records_layout(&KEY, &records_of(&model))
        } else {
            unsealed(&KEY, &records_of(&model))
        };
        let at = flip.index(old.len());
        old[at] ^= 0x5a;
        old.truncate(cut.index(old.len() + 1));
        if let Ok(Some(upgraded)) = upgrade(&KEY, &old) {
            prop_assert!(!sealed);
            let chunk = ChunkRef::parse(&KEY, &upgraded).unwrap();
            prop_assert!(chunk.entries().all(|entry| entry.is_ok()));
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(crate::test_scale::cases(256)))]

    #[test]
    #[ignore = "left out of mise run check, run by mise run random"]
    fn damaged_columns_under_a_valid_checksum_fail_or_read_without_panicking(
        edits in edits(),
        cut in any::<prop::sample::Index>(),
        flip in any::<prop::sample::Index>(),
    ) {
        let (edit, _) = edited(edits);
        let Some(mut stored) = edit.encode(&KEY).unwrap() else {
            return Ok(());
        };
        let at = PRESENT_AT + flip.index(stored.len() - PRESENT_AT);
        stored[at] ^= 0x5a;
        stored.truncate(PRESENT_AT + cut.index(stored.len() - PRESENT_AT + 1));
        let stored = with_checksum(&KEY, stored);
        if let Ok(chunk) = ChunkRef::parse(&KEY, &stored) {
            for slot in 0..CHUNK_SLOTS {
                let _ = chunk.get(slot);
                let _ = chunk.rel_type(slot);
                let _ = chunk.keys(slot);
                for key in 0..6 {
                    let _ = chunk.property(slot, key);
                }
            }
            for (_, record) in chunk.entries().flatten() {
                let _ = RelationshipRecord::decode(&record);
            }
        }
    }
}

fn one_record(rel_type: u32, source: u64, target: u64, properties: Properties) -> Vec<u8> {
    RelationshipRecord {
        rel_type: Token::new(rel_type),
        source: NodeId(source),
        target: NodeId(target),
        properties,
    }
    .encode()
}

#[test]
fn a_relationship_id_splits_into_its_chunk_and_slot() {
    for id in [0, 1, 63, 64, 65, 4095, u64::MAX] {
        let (chunk, slot) = locate(RelationshipId(id));
        assert!(slot < CHUNK_SLOTS);
        assert_eq!(relationship_at(chunk, slot), RelationshipId(id));
    }
    assert_eq!(locate(RelationshipId(66)), (2, 2));
    assert_eq!(parse_key(&chunk_key(7)).unwrap(), 7);
}

#[test]
fn a_chunk_packs_a_full_run_of_one_type_and_one_integer_property_in_a_few_bytes_each() {
    let since = Token::new(1);
    let mut edit = ChunkEdit::default();
    let mut records = 0;
    for slot in 0..CHUNK_SLOTS {
        let source = 10_000 + slot as u64 * 397 % 9_000;
        let target = 20_000 - slot as u64 * 211 % 15_000;
        let properties = Properties::from([(since, PropertyValue::Integer(1990 + slot as i64))]);
        let record = one_record(0, source, target, properties);
        records += record.len() + 1;
        edit.set(slot, Some(&record));
    }
    let stored = edit.encode(&KEY).unwrap().unwrap();
    assert!(
        stored.len() * 2 < records,
        "{} bytes stored for {records} bytes of records",
        stored.len()
    );
}

#[test]
fn a_record_whose_values_differ_in_type_from_its_neighbours_reads_back_bit_for_bit() {
    let key = Token::new(2);
    let mut edit = ChunkEdit::default();
    let values = [
        PropertyValue::Integer(i64::MIN),
        PropertyValue::Integer(i64::MAX),
        PropertyValue::Float(-0.0),
        PropertyValue::String("한국어".to_owned()),
        PropertyValue::Integer(-1),
    ];
    let mut model = Vec::new();
    for (slot, value) in values.into_iter().enumerate() {
        let record = one_record(
            slot as u32,
            u64::MAX - slot as u64,
            slot as u64,
            Properties::from([(key, value)]),
        );
        edit.set(slot * 7, Some(&record));
        model.push((slot * 7, record));
    }
    edit.set(
        30,
        Some(&one_record(u32::MAX, 0, u64::MAX, Properties::new())),
    );
    model.push((30, one_record(u32::MAX, 0, u64::MAX, Properties::new())));
    let stored = edit.encode(&KEY).unwrap().unwrap();
    let read: Vec<(usize, Vec<u8>)> = ChunkRef::parse(&KEY, &stored)
        .unwrap()
        .entries()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(read, model);
}

#[test]
fn a_chunk_of_an_older_layout_with_trailing_bytes_fails_to_upgrade() {
    let records = [(3, one_record(0, 1, 2, Properties::new()))];
    let mut unsealed = unsealed(&KEY, &records);
    unsealed.push(0);
    let mut sealed = in_records_layout(&KEY, &records);
    sealed.push(0);
    assert!(upgrade(&KEY, &unsealed).is_err());
    assert!(upgrade(&KEY, &with_checksum(&KEY, sealed)).is_err());
}

#[test]
fn an_empty_or_unknown_chunk_fails_to_parse() {
    assert!(ChunkRef::parse(&KEY, &[]).is_err());
    assert!(ChunkRef::parse(&KEY, &[3, 0, 0, 0]).is_err());
    assert!(upgrade(&KEY, &[1, 0, 0, 0, 0]).is_err());
    assert!(upgrade(&KEY, &[4, 1, 0, 0, 0, 0]).is_err());
    let older = in_records_layout(&KEY, &[(0, one_record(0, 1, 2, Properties::new()))]);
    assert!(ChunkRef::parse(&KEY, &older).is_err());
    assert!(ChunkRef::parse(&KEY, &upgrade(&KEY, &older).unwrap().unwrap()).is_ok());
}

#[test]
fn every_flipped_bit_of_a_chunk_fails_its_checksum() {
    let mut edit = ChunkEdit::default();
    edit.set(0, Some(&one_record(0, 1, 2, Properties::new())));
    edit.set(
        3,
        Some(&one_record(
            1,
            5,
            9,
            Properties::from([(Token::new(0), PropertyValue::Integer(4))]),
        )),
    );
    edit.set(
        31,
        Some(&one_record(
            0,
            u64::MAX,
            0,
            Properties::from([(Token::new(0), PropertyValue::String("x".repeat(40)))]),
        )),
    );
    let stored = edit.encode(&KEY).unwrap().unwrap();
    for at in 0..stored.len() {
        for bit in 0..8 {
            let mut damaged = stored.clone();
            damaged[at] ^= 1 << bit;
            assert!(
                ChunkRef::parse(&KEY, &damaged).is_err(),
                "byte {at} bit {bit}"
            );
            assert!(
                ChunkEdit::load(&KEY, &damaged).is_err(),
                "byte {at} bit {bit}"
            );
        }
    }
    for cut in 0..stored.len() {
        assert!(
            ChunkRef::parse(&KEY, &stored[..cut]).is_err(),
            "cut at {cut}"
        );
    }
}

#[test]
fn a_chunk_under_another_chunks_key_fails_its_checksum() {
    let mut edit = ChunkEdit::default();
    edit.set(5, Some(&one_record(4, 2, 0, Properties::new())));
    let stored = edit.encode(&KEY).unwrap().unwrap();
    assert!(ChunkRef::parse(&KEY, &stored).is_ok());
    assert!(ChunkRef::parse(&8u64.to_be_bytes(), &stored).is_err());
}

#[test]
fn a_chunk_holding_a_record_that_does_not_decode_fails_to_encode() {
    let mut edit = ChunkEdit::default();
    edit.set(0, Some(&[0xff; 3]));
    assert!(edit.encode(&KEY).is_err());
}

#[test]
fn an_older_record_that_repeats_a_key_upgrades_to_the_value_its_decoding_keeps() {
    for second in [vec![14, 6], vec![14, 200, 1]] {
        let mut record = vec![1, 2, 3, 2, 5, 14, 2, 5];
        record.extend_from_slice(&second);
        let decoded = RelationshipRecord::decode(&record).unwrap();
        let upgraded = upgrade(&KEY, &unsealed(&KEY, &[(0, record)]))
            .unwrap()
            .unwrap();
        let read = ChunkRef::parse(&KEY, &upgraded)
            .unwrap()
            .get(0)
            .unwrap()
            .unwrap();
        assert_eq!(RelationshipRecord::decode(&read).unwrap(), decoded);
    }
}

fn handmade(present: u32, body: &[u8]) -> Vec<u8> {
    let mut stored = vec![LAYOUT_VERSION, 0, 0, 0, 0];
    stored.extend_from_slice(&present.to_le_bytes());
    stored.extend_from_slice(body);
    with_checksum(&KEY, stored)
}

fn reads_as_corrupt(stored: &[u8]) -> bool {
    match ChunkRef::parse(&KEY, stored) {
        Err(_) => true,
        Ok(chunk) => {
            chunk.get(0).is_err()
                && chunk.entries().any(|entry| entry.is_err())
                && chunk.keys(0).is_err()
        }
    }
}

const ONE_RELATIONSHIP_ENDS: [u8; 6] = [0, 0, 0, 0, 0, 0];

#[test]
fn a_handmade_chunk_of_one_integer_column_reads_back() {
    let mut body = ONE_RELATIONSHIP_ENDS.to_vec();
    body.extend_from_slice(&[1, 4, 1, 0, 0, 0, INTEGERS, 6, 0]);
    let chunk = handmade(1, &body);
    let read = ChunkRef::parse(&KEY, &chunk).unwrap();
    assert_eq!(
        read.property(0, 4).unwrap(),
        Some(Some(PropertyValue::Integer(3)))
    );
    assert!(!reads_as_corrupt(&chunk));
}

#[test]
fn a_column_held_by_a_slot_without_a_relationship_reads_as_corrupt() {
    let mut body = ONE_RELATIONSHIP_ENDS.to_vec();
    body.extend_from_slice(&[1, 4, 3, 0, 0, 0, INTEGERS, 6, 0]);
    assert!(reads_as_corrupt(&handmade(1, &body)));
}

#[test]
fn a_packed_column_wider_than_sixty_four_bits_reads_as_corrupt() {
    let mut body = vec![0, 65];
    body.extend_from_slice(&[0; 9]);
    body.extend_from_slice(&[0, 0, 0, 0, 0]);
    assert!(reads_as_corrupt(&handmade(1, &body)));
}

#[test]
fn two_columns_of_one_key_read_as_corrupt() {
    let mut body = ONE_RELATIONSHIP_ENDS.to_vec();
    body.extend_from_slice(&[
        2, 4, 1, 0, 0, 0, INTEGERS, 6, 0, 4, 1, 0, 0, 0, INTEGERS, 8, 0,
    ]);
    assert!(reads_as_corrupt(&handmade(1, &body)));
}

#[test]
fn bytes_past_the_last_column_read_as_corrupt() {
    let mut body = ONE_RELATIONSHIP_ENDS.to_vec();
    body.extend_from_slice(&[1, 4, 1, 0, 0, 0, INTEGERS, 6, 0, 0]);
    assert!(reads_as_corrupt(&handmade(1, &body)));
}
