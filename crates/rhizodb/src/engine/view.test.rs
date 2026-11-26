use std::collections::BTreeMap;

use proptest::collection::btree_map;
use proptest::prelude::*;

use super::*;

fn bytes(value: &[u8]) -> Bytes {
    Arc::from(value)
}

fn slots(map: &BTreeMap<Vec<u8>, Option<Vec<u8>>>) -> Vec<(Bytes, Slot)> {
    map.iter()
        .map(|(key, slot)| (bytes(key), slot.as_deref().map(bytes)))
        .collect()
}

fn key() -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(0u8..4, 0..3)
}

#[test]
fn prefix_ends_skip_trailing_ff() {
    assert_eq!(prefix_end(b""), None);
    assert_eq!(prefix_end(&[0xFF, 0xFF]), None);
    assert_eq!(prefix_end(b"ab"), Some(b"ac".to_vec()));
    assert_eq!(prefix_end(&[1, 0xFF]), Some(vec![2]));
}

#[test]
fn an_error_of_the_visitor_stops_the_merge() {
    let upper = vec![
        (bytes(b"a"), Some(bytes(b"1"))),
        (bytes(b"c"), Some(bytes(b"3"))),
    ];
    let mut seen = Vec::new();
    let result = merge_over(
        &upper,
        |lower| lower(b"b", CellValue::Inline(b"2")),
        &mut |key, _| {
            seen.push(key.to_vec());
            if key == b"b" { Err("stop") } else { Ok(()) }
        },
    );
    assert_eq!(result, Err("stop"));
    assert_eq!(seen, [b"a".to_vec(), b"b".to_vec()]);
}

proptest! {
    #[test]
    #[ignore = "left out of mise run check, run by mise run random"]
    fn merging_slots_lets_the_newer_side_win(
        newer in btree_map(key(), proptest::option::of(key()), 0..8),
        older in btree_map(key(), proptest::option::of(key()), 0..8),
    ) {
        let mut expected = older.clone();
        expected.extend(newer.clone());
        prop_assert_eq!(merge_slots(slots(&newer), slots(&older)), slots(&expected));
    }

    #[test]
    #[ignore = "left out of mise run check, run by mise run random"]
    fn merging_over_a_lower_layer_matches_a_map(
        upper in btree_map(key(), proptest::option::of(key()), 0..8),
        lower in btree_map(key(), key(), 0..8),
    ) {
        let mut expected: BTreeMap<Vec<u8>, Vec<u8>> = lower.clone();
        for (key, slot) in &upper {
            match slot {
                Some(value) => expected.insert(key.clone(), value.clone()),
                None => expected.remove(key),
            };
        }
        let mut merged = Vec::new();
        merge_over::<()>(
            &slots(&upper),
            |visit| {
                for (key, value) in &lower {
                    visit(key, CellValue::Inline(value))?;
                }
                Ok(())
            },
            &mut |key, value| {
                let CellValue::Inline(value) = value else {
                    return Err(());
                };
                merged.push((key.to_vec(), value.to_vec()));
                Ok(())
            },
        )
        .unwrap();
        prop_assert_eq!(merged, expected.into_iter().collect::<Vec<_>>());
    }
}
