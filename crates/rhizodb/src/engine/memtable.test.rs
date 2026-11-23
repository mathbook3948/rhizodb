use std::collections::BTreeMap;

use proptest::collection::vec;
use proptest::prelude::*;

use super::*;

const TABLE: TableId = TableId(0x100);
const OTHER: TableId = TableId(0x101);

fn bytes(value: &[u8]) -> Bytes {
    Arc::from(value)
}

fn put(key: &[u8], value: &[u8]) -> (TableId, Bytes, Slot) {
    (TABLE, bytes(key), Some(bytes(value)))
}

fn delete(key: &[u8]) -> (TableId, Bytes, Slot) {
    (TABLE, bytes(key), None)
}

#[test]
fn a_reader_sees_the_newest_version_at_its_snapshot() {
    let memtable = Memtable::default();
    memtable.apply(1, [put(b"a", b"1")], 0);
    memtable.apply(2, [put(b"a", b"2")], 0);
    memtable.apply(3, [delete(b"a")], 0);
    assert_eq!(memtable.get(TABLE, b"a", 0), Lookup::Absent);
    assert_eq!(memtable.get(TABLE, b"a", 1), Lookup::Found(bytes(b"1")));
    assert_eq!(memtable.get(TABLE, b"a", 2), Lookup::Found(bytes(b"2")));
    assert_eq!(memtable.get(TABLE, b"a", 3), Lookup::Deleted);
    assert_eq!(memtable.get(OTHER, b"a", 3), Lookup::Absent);
    assert_eq!(memtable.get(TABLE, b"b", 3), Lookup::Absent);
}

#[test]
fn pruning_keeps_what_every_snapshot_from_the_horizon_on_sees() {
    let memtable = Memtable::default();
    for lsn in 1..=10u64 {
        memtable.apply(lsn, [put(b"k", &lsn.to_be_bytes())], 4);
    }
    for snapshot in 4..=10u64 {
        assert_eq!(
            memtable.get(TABLE, b"k", snapshot),
            Lookup::Found(bytes(&snapshot.to_be_bytes()))
        );
    }
    assert_eq!(memtable.get(TABLE, b"k", 3), Lookup::Absent);
    memtable.apply(11, [delete(b"k")], 11);
    assert_eq!(memtable.get(TABLE, b"k", 11), Lookup::Deleted);
    assert_eq!(memtable.get(TABLE, b"k", 10), Lookup::Absent);
}

#[test]
fn pruning_frees_the_bytes_it_drops() {
    let memtable = Memtable::default();
    let value = vec![0u8; 1000];
    memtable.apply(1, [put(b"k", &value)], 0);
    let one = memtable.bytes();
    assert!(one >= 1000);
    for lsn in 2..100 {
        memtable.apply(lsn, [put(b"k", &value)], lsn);
    }
    assert_eq!(memtable.bytes(), one);
    memtable.apply(100, [put(b"k", &value)], 0);
    assert_eq!(memtable.bytes(), 2 * one);
}

#[test]
fn a_range_holds_tombstones_and_respects_bounds() {
    let memtable = Memtable::default();
    memtable.apply(1, [put(b"a", b"1"), put(b"b", b"2"), put(b"c", b"3")], 0);
    memtable.apply(2, [delete(b"b")], 0);
    let range = memtable.range(TABLE, Bound::Included(b"a"), Bound::Excluded(b"c"), 2);
    assert_eq!(
        range,
        [(bytes(b"a"), Some(bytes(b"1"))), (bytes(b"b"), None)]
    );
    let before = memtable.range(TABLE, Bound::Unbounded, Bound::Unbounded, 1);
    assert_eq!(before.len(), 3);
    assert!(before.iter().all(|(_, slot)| slot.is_some()));
    for (start, end) in [
        (
            Bound::Included(b"c".as_slice()),
            Bound::Excluded(b"a".as_slice()),
        ),
        (
            Bound::Excluded(b"b".as_slice()),
            Bound::Excluded(b"b".as_slice()),
        ),
        (
            Bound::Included(b"b".as_slice()),
            Bound::Excluded(b"b".as_slice()),
        ),
    ] {
        assert!(memtable.range(TABLE, start, end, 2).is_empty());
    }
}

#[test]
fn the_newest_versions_are_what_a_checkpoint_writes() {
    let memtable = Memtable::default();
    assert!(memtable.is_empty());
    memtable.apply(1, [put(b"a", b"1"), (OTHER, bytes(b"x"), None)], 0);
    memtable.apply(2, [put(b"a", b"2")], 0);
    assert!(!memtable.is_empty());
    let newest = memtable.newest();
    assert_eq!(newest[&TABLE], [(bytes(b"a"), Some(bytes(b"2")))]);
    assert_eq!(newest[&OTHER], [(bytes(b"x"), None)]);
}

#[test]
fn ordered_bounds_match_what_a_btree_map_accepts() {
    let keys: [&[u8]; 3] = [b"", b"a", b"b"];
    for start in keys {
        for end in keys {
            for start in [
                Bound::Included(start),
                Bound::Excluded(start),
                Bound::Unbounded,
            ] {
                for end in [Bound::Included(end), Bound::Excluded(end), Bound::Unbounded] {
                    let map: BTreeMap<Vec<u8>, ()> = BTreeMap::new();
                    let accepted =
                        std::panic::catch_unwind(|| map.range::<[u8], _>((start, end)).count())
                            .is_ok();
                    if bounds_are_ordered(start, end) {
                        assert!(accepted, "{start:?} {end:?}");
                    }
                }
            }
        }
    }
}

proptest! {
    #[test]
    fn every_snapshot_at_or_after_the_horizon_reads_its_own_state(
        commits in vec(vec((0u8..6, proptest::option::of(any::<u8>())), 1..5), 1..30),
        horizon_lag in 0u64..10,
    ) {
        let memtable = Memtable::default();
        let mut states: Vec<BTreeMap<u8, u8>> = vec![BTreeMap::new()];
        for (index, writes) in commits.iter().enumerate() {
            let lsn = index as u64 + 1;
            let mut state = states.last().cloned().unwrap_or_default();
            let mut changes = BTreeMap::new();
            for (key, value) in writes {
                match value {
                    Some(value) => state.insert(*key, *value),
                    None => state.remove(key),
                };
                changes.insert(*key, *value);
            }
            states.push(state);
            let horizon = (lsn - 1).saturating_sub(horizon_lag);
            memtable.apply(
                lsn,
                changes.into_iter().map(|(key, value)| {
                    (TABLE, bytes(&[key]), value.map(|value| bytes(&[value])))
                }),
                horizon,
            );
        }
        let last = states.len() as u64 - 1;
        for snapshot in last.saturating_sub(horizon_lag)..=last {
            let state = &states[snapshot as usize];
            for key in 0u8..6 {
                let found = match memtable.get(TABLE, &[key], snapshot) {
                    Lookup::Found(value) => Some(value[0]),
                    Lookup::Deleted | Lookup::Absent => None,
                };
                prop_assert_eq!(found, state.get(&key).copied());
            }
            let ranged: BTreeMap<u8, u8> = memtable
                .range(TABLE, Bound::Unbounded, Bound::Unbounded, snapshot)
                .into_iter()
                .filter_map(|(key, slot)| slot.map(|value| (key[0], value[0])))
                .collect();
            prop_assert_eq!(&ranged, state);
        }
    }
}
