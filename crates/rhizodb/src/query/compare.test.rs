use std::cmp::Ordering;
use std::sync::Arc;

use proptest::prelude::*;

use super::*;

fn scalar() -> impl Strategy<Value = Datum> {
    prop_oneof![
        Just(Datum::Null),
        any::<bool>().prop_map(Datum::Boolean),
        any::<i64>().prop_map(Datum::Integer),
        (-3i64..3).prop_map(Datum::Integer),
        any::<f64>().prop_map(Datum::Float),
        (-3i64..3).prop_map(|value| Datum::Float(value as f64)),
        "[ab]{0,2}".prop_map(Datum::String),
    ]
}

fn datum() -> impl Strategy<Value = Datum> {
    scalar().prop_recursive(3, 12, 3, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..3)
                .prop_map(|items| Datum::List(Arc::new(items))),
            prop::collection::btree_map("[ab]", inner, 0..2)
                .prop_map(|entries| Datum::Map(Arc::new(entries))),
        ]
    })
}

proptest! {
    #[test]
    fn order_is_antisymmetric(a in datum(), b in datum()) {
        prop_assert_eq!(order(&a, &b), order(&b, &a).reverse());
    }

    #[test]
    fn order_is_transitive(a in datum(), b in datum(), c in datum()) {
        if order(&a, &b) != Ordering::Greater && order(&b, &c) != Ordering::Greater {
            prop_assert_ne!(order(&a, &c), Ordering::Greater);
        }
    }

    #[test]
    fn equal_values_are_equivalent(a in datum(), b in datum()) {
        if equals(&a, &b) == Some(true) {
            prop_assert_eq!(order(&a, &b), Ordering::Equal);
        }
    }
}

#[test]
fn integers_and_floats_compare_exactly_beyond_float_precision() {
    let big = Datum::Integer(i64::MAX);
    let rounded = Datum::Float(i64::MAX as f64);
    assert_eq!(equals(&big, &rounded), Some(false));
    assert_eq!(
        compare(&big, &rounded),
        Some(Comparison::Ordered(Ordering::Less))
    );
}

#[test]
fn the_global_order_ranks_types() {
    let ascending = [
        Datum::Map(Arc::default()),
        Datum::List(Arc::default()),
        Datum::String(String::new()),
        Datum::Boolean(false),
        Datum::Integer(1),
        Datum::Float(f64::NAN),
        Datum::Null,
    ];
    for pair in ascending.windows(2) {
        assert_eq!(order(&pair[0], &pair[1]), Ordering::Less, "{pair:?}");
    }
}
