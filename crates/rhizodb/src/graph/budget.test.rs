use std::collections::BTreeMap;

use proptest::prelude::*;

use super::*;

#[derive(Debug, PartialEq)]
struct Bytes(usize);

impl Weigh for Bytes {
    fn weight(&self) -> usize {
        self.0
    }
}

const ENTRY: usize = size_of::<(u64, Entry<Bytes>)>();

fn held(map: &Budgeted<u64, Bytes>) -> usize {
    map.values().map(|value| ENTRY + value.0).sum()
}

#[test]
fn a_map_filled_in_random_order_holds_its_budget_and_its_newest_entries() {
    let budget = 64 << 10;
    let mut map = Budgeted::new(budget);
    for at in 0..20_000_u64 {
        let key = at.wrapping_mul(7919) % 20_000;
        map.insert(key, Bytes(1000));
        assert!(map.bytes() <= budget, "{} bytes after {at}", map.bytes());
        assert_eq!(
            map.get(&key),
            Some(&mut Bytes(1000)),
            "the newest entry stays"
        );
    }
    assert_eq!(map.bytes(), held(&map));
    assert!(map.bytes() >= budget / 2 - 1000 - ENTRY, "{}", map.bytes());
}

#[test]
fn a_map_drops_the_entries_used_longest_ago_first() {
    let mut map = Budgeted::new(10 * (ENTRY + 100));
    for key in 0..10 {
        map.insert(key, Bytes(100));
    }
    map.get(&0);
    map.insert(10, Bytes(100));
    let kept: Vec<u64> = (0..=10).filter(|key| map.get(key).is_some()).collect();
    assert_eq!(kept, [0, 7, 8, 9, 10]);
}

#[test]
fn an_entry_larger_than_the_budget_stays_until_the_next_insert() {
    let mut map = Budgeted::new(1000);
    map.insert(1, Bytes(10));
    map.insert(2, Bytes(5000));
    assert_eq!(map.get(&1), None);
    assert_eq!(map.get(&2), Some(&mut Bytes(5000)));
    map.insert(3, Bytes(10));
    assert_eq!(map.get(&2), None);
    assert_eq!(map.bytes(), ENTRY + 10);
}

#[test]
fn inserting_a_held_key_replaces_its_value_and_weight() {
    let mut map = Budgeted::new(1 << 20);
    map.insert(1, Bytes(10));
    map.insert(1, Bytes(300));
    assert_eq!(map.get(&1), Some(&mut Bytes(300)));
    assert_eq!(map.bytes(), ENTRY + 300);
}

proptest! {
    #[test]
    fn a_map_holds_what_a_model_of_least_recent_use_holds_within_its_budget(
        steps in prop::collection::vec((0..64_u64, prop::option::of(0..2000_usize)), 1..400),
    ) {
        let budget = 16 * (ENTRY + 500);
        let mut map = Budgeted::new(budget);
        let mut model: BTreeMap<u64, usize> = BTreeMap::new();
        for (key, insert) in steps {
            match insert {
                Some(weight) => {
                    map.insert(key, Bytes(weight));
                    model.insert(key, weight);
                    prop_assert_eq!(map.get(&key), Some(&mut Bytes(weight)));
                }
                None => {
                    if let Some(value) = map.get(&key) {
                        prop_assert_eq!(Some(&value.0), model.get(&key), "a held value is the last inserted");
                    }
                }
            }
            prop_assert_eq!(map.bytes(), held(&map));
            let largest = map.values().map(|value| ENTRY + value.0).max().unwrap_or(0);
            prop_assert!(map.bytes() <= budget.max(largest), "{} bytes", map.bytes());
        }
    }
}
