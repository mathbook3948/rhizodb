use std::sync::Arc;

use proptest::prelude::*;

use super::*;
use crate::graph::NodeId;
use crate::test_scale;

const FLOATS: [f64; 14] = [
    0.0,
    -0.0,
    1.0,
    -1.0,
    2.5,
    f64::NAN,
    -f64::NAN,
    f64::INFINITY,
    f64::NEG_INFINITY,
    9_223_372_036_854_775_808.0,
    -9_223_372_036_854_775_808.0,
    9_223_372_036_854_774_784.0,
    4_611_686_018_427_387_904.5,
    1e300,
];

const INTEGERS: [i64; 7] = [0, 1, -1, 2, i64::MIN, i64::MAX, 9_223_372_036_854_774_784];

fn datum() -> impl Strategy<Value = Datum> {
    let leaf = prop_oneof![
        Just(Datum::Null),
        any::<bool>().prop_map(Datum::Boolean),
        prop::sample::select(INTEGERS.to_vec()).prop_map(Datum::Integer),
        prop::sample::select(FLOATS.to_vec()).prop_map(Datum::Float),
        prop::sample::select(vec!["", "a", "b", "ab"]).prop_map(|text| Datum::String(text.into())),
        prop::sample::select(vec![vec![], vec![0u8], vec![1u8]]).prop_map(Datum::Bytes),
        (0u64..3).prop_map(|id| Datum::Node(NodeId(id))),
    ];
    leaf.prop_recursive(2, 6, 3, |inner| {
        prop::collection::vec(inner, 0..3).prop_map(|items| Datum::List(Arc::new(items)))
    })
}

fn row() -> impl Strategy<Value = Vec<Datum>> {
    prop::collection::vec(datum(), 1..3)
}

#[test]
fn equal_integers_and_floats_share_a_hash() {
    let state = Seed::default();
    for (integer, float) in [
        (0, 0.0),
        (0, -0.0),
        (1, 1.0),
        (-9_223_372_036_854_775_808, -9_223_372_036_854_775_808.0),
    ] {
        assert_eq!(
            hash(&state, &[Datum::Integer(integer)]),
            hash(&state, &[Datum::Float(float)])
        );
    }
    assert_eq!(
        hash(&state, &[Datum::Float(f64::NAN)]),
        hash(&state, &[Datum::Float(-f64::NAN)])
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(test_scale::cases(512)))]

    #[test]
    #[ignore = "left out of mise run check, run by mise run random"]
    fn rows_are_found_exactly_when_order_calls_them_equal(rows in prop::collection::vec(row(), 1..12)) {
        let mut index = KeyIndex::default();
        let mut distinct: Vec<Vec<Datum>> = Vec::new();
        for (position, row) in rows.iter().enumerate() {
            let expected = distinct
                .iter()
                .position(|held| Key(held.clone()) == Key(row.clone()));
            let found = index.get(row);
            prop_assert_eq!(found, expected, "row {} {:?} among {:?}", position, row, distinct);
            if found.is_none() {
                index.insert(row.clone(), distinct.len());
                distinct.push(row.clone());
            }
        }
    }

    #[test]
    #[ignore = "left out of mise run check, run by mise run random"]
    fn equivalent_rows_share_a_hash(a in row(), b in row()) {
        if Key(a.clone()) == Key(b.clone()) {
            let state = Seed::default();
            prop_assert_eq!(hash(&state, &a), hash(&state, &b));
        }
    }
}
