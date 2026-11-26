use std::io;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use super::Entry;
use super::ReadTransaction;
use super::Storage;
use super::StorageError;
use super::Table;
use super::WriteTransaction;

/// Opens one backend's store for the conformance suite and the model test.
pub(crate) trait Harness {
    type Store: Storage + Sync;

    /// Opens the store, empty the first time and holding every earlier commit after.
    fn open(&mut self) -> Self::Store;
}

type Case<H> = (&'static str, fn(&mut H));

/// Runs every conformance case on a store from a fresh harness, and fails naming each case
/// that failed.
pub(crate) fn run<H: Harness>(mut fresh: impl FnMut() -> H) {
    let cases: [Case<H>; 19] = [
        ("reads_what_was_committed", reads_what_was_committed),
        ("a_put_replaces_the_value", a_put_replaces_the_value),
        ("a_delete_removes_its_key", a_delete_removes_its_key),
        (
            "a_range_delete_removes_keys_from_start_to_end",
            a_range_delete_removes_keys_from_start_to_end,
        ),
        (
            "every_table_keeps_its_own_entries",
            every_table_keeps_its_own_entries,
        ),
        (
            "empty_keys_and_values_are_stored",
            empty_keys_and_values_are_stored,
        ),
        (
            "large_values_and_long_keys_round_trip",
            large_values_and_long_keys_round_trip,
        ),
        (
            "a_prefix_scan_yields_matching_keys_in_order",
            a_prefix_scan_yields_matching_keys_in_order,
        ),
        (
            "a_range_scan_yields_keys_from_start_to_end",
            a_range_scan_yields_keys_from_start_to_end,
        ),
        (
            "a_visitor_error_stops_the_visit",
            a_visitor_error_stops_the_visit,
        ),
        (
            "get_with_hands_over_the_value_and_errors",
            get_with_hands_over_the_value_and_errors,
        ),
        (
            "a_write_transaction_reads_its_own_writes",
            a_write_transaction_reads_its_own_writes,
        ),
        (
            "rollback_and_drop_discard_writes",
            rollback_and_drop_discard_writes,
        ),
        (
            "a_reader_keeps_its_snapshot_across_commits",
            a_reader_keeps_its_snapshot_across_commits,
        ),
        (
            "a_reader_begun_during_a_write_does_not_see_it",
            a_reader_begun_during_a_write_does_not_see_it,
        ),
        (
            "a_reader_keeps_its_snapshot_while_another_thread_commits",
            a_reader_keeps_its_snapshot_while_another_thread_commits,
        ),
        (
            "a_commit_becomes_visible_at_once_and_whole",
            a_commit_becomes_visible_at_once_and_whole,
        ),
        (
            "a_second_writer_waits_for_the_first",
            a_second_writer_waits_for_the_first,
        ),
        (
            "commits_survive_a_reopen_and_the_rest_does_not",
            commits_survive_a_reopen_and_the_rest_does_not,
        ),
    ];
    let mut failed = Vec::new();
    for (name, case) in cases {
        let mut harness = fresh();
        eprintln!("storage conformance case {name}");
        if std::panic::catch_unwind(AssertUnwindSafe(|| case(&mut harness))).is_err() {
            failed.push(name);
        }
    }
    assert!(
        failed.is_empty(),
        "storage conformance cases failed: {failed:?}"
    );
}

fn put_all(store: &impl Storage, table: Table, entries: &[(&[u8], &[u8])]) {
    let mut write = store.begin_write().unwrap();
    for (key, value) in entries {
        write.put(table, key, value).unwrap();
    }
    write.commit().unwrap();
}

fn latest(store: &impl Storage, table: Table, key: &[u8]) -> Option<Vec<u8>> {
    store.begin_read().unwrap().get(table, key).unwrap()
}

fn dump(read: &impl ReadTransaction, table: Table) -> Vec<Entry> {
    read.scan_prefix(table, &[]).unwrap()
}

fn entries(pairs: &[(&[u8], &[u8])]) -> Vec<Entry> {
    pairs
        .iter()
        .map(|(key, value)| (key.to_vec(), value.to_vec()))
        .collect()
}

fn keys(found: Vec<Entry>) -> Vec<Vec<u8>> {
    found.into_iter().map(|(key, _)| key).collect()
}

fn visited(read: &impl ReadTransaction, table: Table, prefix: &[u8]) -> Vec<Entry> {
    let mut found = Vec::new();
    read.visit_prefix(table, prefix, |key, value| {
        found.push((key.to_vec(), value.to_vec()));
        Ok::<_, StorageError>(())
    })
    .unwrap();
    found
}

fn visited_range(
    read: &impl ReadTransaction,
    table: Table,
    start: &[u8],
    end: &[u8],
) -> Vec<Entry> {
    let mut found = Vec::new();
    read.visit_range(table, start, end, |key, value| {
        found.push((key.to_vec(), value.to_vec()));
        Ok::<_, StorageError>(())
    })
    .unwrap();
    found
}

fn copied(read: &impl ReadTransaction, table: Table, key: &[u8]) -> Option<Vec<u8>> {
    read.get_with(table, key, |value| Ok::<_, StorageError>(value.to_vec()))
        .unwrap()
}

fn patterned(length: usize, seed: u8) -> Vec<u8> {
    (0..length)
        .map(|index| (index % 251) as u8 ^ seed)
        .collect()
}

fn stopped() -> StorageError {
    StorageError::new(io::Error::other("the visitor stopped"))
}

fn reads_what_was_committed<H: Harness>(harness: &mut H) {
    let store = harness.open();
    put_all(&store, Table::Nodes, &[(b"a", b"1"), (b"b", b"2")]);
    let read = store.begin_read().unwrap();
    assert_eq!(read.get(Table::Nodes, b"a").unwrap(), Some(b"1".to_vec()));
    assert_eq!(read.get(Table::Nodes, b"b").unwrap(), Some(b"2".to_vec()));
    assert_eq!(read.get(Table::Nodes, b"c").unwrap(), None);
    assert_eq!(read.get(Table::RelationshipChunks, b"a").unwrap(), None);
    assert_eq!(copied(&read, Table::Nodes, b"a"), Some(b"1".to_vec()));
}

fn a_put_replaces_the_value<H: Harness>(harness: &mut H) {
    let store = harness.open();
    put_all(&store, Table::Nodes, &[(b"a", b"1")]);
    put_all(&store, Table::Nodes, &[(b"a", b"22")]);
    let mut write = store.begin_write().unwrap();
    write.put(Table::Nodes, b"a", b"333").unwrap();
    write.put(Table::Nodes, b"a", b"4").unwrap();
    write.commit().unwrap();
    assert_eq!(
        dump(&store.begin_read().unwrap(), Table::Nodes),
        entries(&[(b"a", b"4")])
    );
}

fn a_delete_removes_its_key<H: Harness>(harness: &mut H) {
    let store = harness.open();
    put_all(&store, Table::Nodes, &[(b"a", b"1"), (b"b", b"2")]);
    let mut write = store.begin_write().unwrap();
    write.delete(Table::Nodes, b"a").unwrap();
    write.delete(Table::Nodes, b"missing").unwrap();
    write.delete(Table::RelationshipChunks, b"b").unwrap();
    write.commit().unwrap();
    assert_eq!(
        dump(&store.begin_read().unwrap(), Table::Nodes),
        entries(&[(b"b", b"2")])
    );

    let mut write = store.begin_write().unwrap();
    write.delete(Table::Nodes, b"b").unwrap();
    write.put(Table::Nodes, b"b", b"3").unwrap();
    write.put(Table::Nodes, b"c", b"4").unwrap();
    write.delete(Table::Nodes, b"c").unwrap();
    write.commit().unwrap();
    assert_eq!(
        dump(&store.begin_read().unwrap(), Table::Nodes),
        entries(&[(b"b", b"3")])
    );
}

fn a_range_delete_removes_keys_from_start_to_end<H: Harness>(harness: &mut H) {
    let store = harness.open();
    let all: [(&[u8], &[u8]); 6] = [
        (b"a", b"1"),
        (b"b", b"2"),
        (b"ba", b"3"),
        (b"bb", b"4"),
        (b"c", b"5"),
        (b"\xFF", b"6"),
    ];
    put_all(&store, Table::NodeColumns, &all);
    put_all(&store, Table::NodeChunks, &all);
    let mut write = store.begin_write().unwrap();
    write.delete_range(Table::NodeColumns, b"c", b"b").unwrap();
    write.delete_range(Table::NodeColumns, b"b", b"b").unwrap();
    write.delete_range(Table::NodeColumns, b"d", b"e").unwrap();
    assert_eq!(
        dump(&write, Table::NodeColumns),
        entries(&all),
        "an empty or reversed range deletes nothing"
    );
    write.delete_range(Table::NodeColumns, b"b", b"bb").unwrap();
    assert_eq!(
        dump(&write, Table::NodeColumns),
        entries(&[(b"a", b"1"), (b"bb", b"4"), (b"c", b"5"), (b"\xFF", b"6")]),
        "the write sees its own range delete"
    );
    write.put(Table::NodeColumns, b"b", b"7").unwrap();
    write.commit().unwrap();
    let read = store.begin_read().unwrap();
    assert_eq!(
        dump(&read, Table::NodeColumns),
        entries(&[
            (b"a", b"1"),
            (b"b", b"7"),
            (b"bb", b"4"),
            (b"c", b"5"),
            (b"\xFF", b"6")
        ])
    );
    assert_eq!(
        dump(&read, Table::NodeChunks),
        entries(&all),
        "other tables keep theirs"
    );
    drop(read);
    let mut write = store.begin_write().unwrap();
    write
        .delete_range(Table::NodeColumns, b"", b"\xFF\xFF")
        .unwrap();
    write.rollback().unwrap();
    assert_eq!(
        dump(&store.begin_read().unwrap(), Table::NodeColumns).len(),
        5
    );
    let mut write = store.begin_write().unwrap();
    write
        .delete_range(Table::NodeColumns, b"", b"\xFF\xFF")
        .unwrap();
    write.commit().unwrap();
    assert!(dump(&store.begin_read().unwrap(), Table::NodeColumns).is_empty());
}

fn every_table_keeps_its_own_entries<H: Harness>(harness: &mut H) {
    let store = harness.open();
    let mut write = store.begin_write().unwrap();
    for table in Table::ALL {
        write.put(table, b"key", table.name().as_bytes()).unwrap();
    }
    write.commit().unwrap();
    let mut write = store.begin_write().unwrap();
    write.delete(Table::Metadata, b"key").unwrap();
    write.commit().unwrap();

    let read = store.begin_read().unwrap();
    assert!(dump(&read, Table::Metadata).is_empty());
    for table in &Table::ALL[1..] {
        assert_eq!(
            dump(&read, *table),
            entries(&[(b"key", table.name().as_bytes())])
        );
    }
}

fn empty_keys_and_values_are_stored<H: Harness>(harness: &mut H) {
    let store = harness.open();
    put_all(&store, Table::Indexes, &[(b"", b"empty key"), (b"k", b"")]);
    let read = store.begin_read().unwrap();
    assert_eq!(
        read.get(Table::Indexes, b"").unwrap(),
        Some(b"empty key".to_vec())
    );
    assert_eq!(read.get(Table::Indexes, b"k").unwrap(), Some(Vec::new()));
    assert_eq!(
        dump(&read, Table::Indexes),
        entries(&[(b"", b"empty key"), (b"k", b"")])
    );
}

fn large_values_and_long_keys_round_trip<H: Harness>(harness: &mut H) {
    let store = harness.open();
    let lengths = [1023, 1024, 1025, 4095, 4096, 4097, 3 * 4096 + 1, 1 << 20];
    let values: Vec<Vec<u8>> = lengths
        .iter()
        .enumerate()
        .map(|(index, length)| patterned(*length, index as u8))
        .collect();
    let long_key = patterned(3000, 7);
    let mut write = store.begin_write().unwrap();
    for (index, value) in values.iter().enumerate() {
        write.put(Table::Vectors, &[index as u8], value).unwrap();
    }
    write.put(Table::Vectors, &long_key, b"long").unwrap();
    write.commit().unwrap();

    let read = store.begin_read().unwrap();
    for (index, value) in values.iter().enumerate() {
        assert_eq!(
            read.get(Table::Vectors, &[index as u8]).unwrap().as_ref(),
            Some(value)
        );
    }
    assert_eq!(
        read.get(Table::Vectors, &long_key).unwrap(),
        Some(b"long".to_vec())
    );
    assert_eq!(keys(dump(&read, Table::Vectors)).last(), Some(&long_key));

    let mut write = store.begin_write().unwrap();
    write.put(Table::Vectors, &[7], b"small").unwrap();
    write.put(Table::Vectors, &[0], &values[7]).unwrap();
    write.commit().unwrap();
    let read = store.begin_read().unwrap();
    assert_eq!(
        read.get(Table::Vectors, &[7]).unwrap(),
        Some(b"small".to_vec())
    );
    assert_eq!(
        read.get(Table::Vectors, &[0]).unwrap().as_ref(),
        Some(&values[7])
    );
}

fn a_prefix_scan_yields_matching_keys_in_order<H: Harness>(harness: &mut H) {
    let store = harness.open();
    let stored: [&[u8]; 9] = [
        &[2],
        &[0xFF, 0xFF],
        &[1, 0xFF],
        &[0xFF],
        &[1],
        &[0xFF, 0x00],
        &[1, 0],
        &[0, 5],
        &[0xFE, 0xFF],
    ];
    let pairs: Vec<(&[u8], &[u8])> = stored.iter().map(|key| (*key, *key)).collect();
    put_all(&store, Table::Adjacency, &pairs);

    let read = store.begin_read().unwrap();
    let scanned = |prefix: &[u8]| keys(read.scan_prefix(Table::Adjacency, prefix).unwrap());
    assert_eq!(scanned(&[1]), [vec![1], vec![1, 0], vec![1, 0xFF]]);
    assert_eq!(
        scanned(&[0xFF]),
        [vec![0xFF], vec![0xFF, 0x00], vec![0xFF, 0xFF]]
    );
    assert_eq!(scanned(&[0xFF, 0xFF]), [vec![0xFF, 0xFF]]);
    assert_eq!(scanned(&[0xFE]), [vec![0xFE, 0xFF]]);
    assert!(scanned(&[3]).is_empty());
    assert!(scanned(&[1, 0, 0]).is_empty());
    let mut all: Vec<Vec<u8>> = stored.iter().map(|key| key.to_vec()).collect();
    all.sort();
    assert_eq!(scanned(&[]), all);
    for prefix in [&[][..], &[1], &[0xFF], &[3]] {
        assert_eq!(
            visited(&read, Table::Adjacency, prefix),
            read.scan_prefix(Table::Adjacency, prefix).unwrap()
        );
    }
    assert!(dump(&read, Table::NodeColumns).is_empty());
}

fn a_range_scan_yields_keys_from_start_to_end<H: Harness>(harness: &mut H) {
    let store = harness.open();
    put_all(
        &store,
        Table::PropertyEntries,
        &[
            (b"a", b"1"),
            (b"b", b"2"),
            (b"ba", b"3"),
            (b"c", b"4"),
            (b"\xFF", b"5"),
        ],
    );
    let read = store.begin_read().unwrap();
    let ranged = |start: &[u8], end: &[u8]| {
        let scanned = read.scan_range(Table::PropertyEntries, start, end).unwrap();
        assert_eq!(
            visited_range(&read, Table::PropertyEntries, start, end),
            scanned,
            "a visit of {start:?}..{end:?} yields what a scan does"
        );
        keys(scanned)
    };
    assert_eq!(ranged(b"b", b"c"), [b"b".to_vec(), b"ba".to_vec()]);
    assert_eq!(ranged(b"a", b"b"), [b"a".to_vec()]);
    assert_eq!(ranged(b"aa", b"bb"), [b"b".to_vec(), b"ba".to_vec()]);
    assert_eq!(ranged(b"", b"\xFF\xFF").len(), 5);
    assert_eq!(ranged(b"c", b"\xFF"), [b"c".to_vec()]);
    assert!(ranged(b"c", b"b").is_empty());
    assert!(ranged(b"b", b"b").is_empty());
    assert!(ranged(b"d", b"e").is_empty());
}

fn a_visitor_error_stops_the_visit<H: Harness>(harness: &mut H) {
    let store = harness.open();
    put_all(
        &store,
        Table::Nodes,
        &[(b"a", b"1"), (b"b", b"2"), (b"c", b"3")],
    );
    let read = store.begin_read().unwrap();
    let mut seen = 0;
    let result = read.visit_prefix(Table::Nodes, &[], |_, _| {
        seen += 1;
        if seen == 2 { Err(stopped()) } else { Ok(()) }
    });
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("the visitor stopped")
    );
    assert_eq!(seen, 2);
    let mut seen = 0;
    let result = read.visit_range(Table::Nodes, b"a", b"z", |_, _| {
        seen += 1;
        if seen == 2 { Err(stopped()) } else { Ok(()) }
    });
    assert!(result.is_err());
    assert_eq!(seen, 2);
}

fn get_with_hands_over_the_value_and_errors<H: Harness>(harness: &mut H) {
    let store = harness.open();
    put_all(&store, Table::Nodes, &[(b"a", b"12345")]);
    let read = store.begin_read().unwrap();
    let length = read.get_with(Table::Nodes, b"a", |value| {
        Ok::<_, StorageError>(value.len())
    });
    assert_eq!(length.unwrap(), Some(5));
    let mut called = false;
    let missing = read.get_with(Table::Nodes, b"b", |_| {
        called = true;
        Ok::<_, StorageError>(())
    });
    assert_eq!(missing.unwrap(), None);
    assert!(!called);
    let failed = read.get_with(Table::Nodes, b"a", |_| Err::<(), _>(stopped()));
    assert!(
        failed
            .unwrap_err()
            .to_string()
            .contains("the visitor stopped")
    );
}

fn a_write_transaction_reads_its_own_writes<H: Harness>(harness: &mut H) {
    let store = harness.open();
    put_all(
        &store,
        Table::LabelIndex,
        &[(b"a1", b"old"), (b"a2", b"old"), (b"b1", b"old")],
    );
    let mut write = store.begin_write().unwrap();
    write.put(Table::LabelIndex, b"a1", b"new").unwrap();
    write.delete(Table::LabelIndex, b"a2").unwrap();
    write.put(Table::LabelIndex, b"a3", b"added").unwrap();

    let expected = entries(&[(b"a1", b"new"), (b"a3", b"added")]);
    assert_eq!(
        write.get(Table::LabelIndex, b"a1").unwrap(),
        Some(b"new".to_vec())
    );
    assert_eq!(write.get(Table::LabelIndex, b"a2").unwrap(), None);
    assert_eq!(
        copied(&write, Table::LabelIndex, b"a3"),
        Some(b"added".to_vec())
    );
    assert_eq!(
        write.scan_prefix(Table::LabelIndex, b"a").unwrap(),
        expected
    );
    assert_eq!(visited(&write, Table::LabelIndex, b"a"), expected);
    assert_eq!(
        write.scan_range(Table::LabelIndex, b"a", b"b").unwrap(),
        expected
    );
    assert_eq!(
        visited_range(&write, Table::LabelIndex, b"a", b"b"),
        expected
    );
    assert_eq!(
        latest(&store, Table::LabelIndex, b"a1"),
        Some(b"old".to_vec())
    );
    write.commit().unwrap();
    assert_eq!(
        dump(&store.begin_read().unwrap(), Table::LabelIndex),
        entries(&[(b"a1", b"new"), (b"a3", b"added"), (b"b1", b"old")])
    );
}

fn rollback_and_drop_discard_writes<H: Harness>(harness: &mut H) {
    let store = harness.open();
    put_all(&store, Table::Nodes, &[(b"a", b"1")]);
    let mut write = store.begin_write().unwrap();
    write.put(Table::Nodes, b"a", b"2").unwrap();
    write.put(Table::Nodes, b"b", b"2").unwrap();
    write.rollback().unwrap();
    {
        let mut write = store.begin_write().unwrap();
        write.delete(Table::Nodes, b"a").unwrap();
        write.put(Table::RelationshipChunks, b"c", b"3").unwrap();
    }
    let read = store.begin_read().unwrap();
    assert_eq!(dump(&read, Table::Nodes), entries(&[(b"a", b"1")]));
    assert!(dump(&read, Table::RelationshipChunks).is_empty());

    let mut write = store.begin_write().unwrap();
    assert_eq!(write.get(Table::Nodes, b"a").unwrap(), Some(b"1".to_vec()));
    write.put(Table::Nodes, b"b", b"4").unwrap();
    write.commit().unwrap();
    store.begin_write().unwrap().commit().unwrap();
    assert_eq!(
        dump(&store.begin_read().unwrap(), Table::Nodes),
        entries(&[(b"a", b"1"), (b"b", b"4")])
    );
}

fn a_reader_keeps_its_snapshot_across_commits<H: Harness>(harness: &mut H) {
    let store = harness.open();
    put_all(&store, Table::Nodes, &[(b"a", b"1"), (b"c", b"1")]);
    let first = store.begin_read().unwrap();
    let mut write = store.begin_write().unwrap();
    write.put(Table::Nodes, b"a", b"2").unwrap();
    write.put(Table::Nodes, b"b", b"2").unwrap();
    write.delete(Table::Nodes, b"c").unwrap();
    write.commit().unwrap();
    let second = store.begin_read().unwrap();
    put_all(&store, Table::Nodes, &[(b"a", b"3"), (b"d", b"3")]);
    let third = store.begin_read().unwrap();

    assert_eq!(
        dump(&first, Table::Nodes),
        entries(&[(b"a", b"1"), (b"c", b"1")])
    );
    assert_eq!(
        dump(&second, Table::Nodes),
        entries(&[(b"a", b"2"), (b"b", b"2")])
    );
    assert_eq!(
        dump(&third, Table::Nodes),
        entries(&[(b"a", b"3"), (b"b", b"2"), (b"d", b"3")])
    );
    assert_eq!(first.get(Table::Nodes, b"c").unwrap(), Some(b"1".to_vec()));
    assert_eq!(copied(&second, Table::Nodes, b"c"), None);
}

fn a_reader_begun_during_a_write_does_not_see_it<H: Harness>(harness: &mut H) {
    let store = harness.open();
    put_all(&store, Table::Nodes, &[(b"a", b"1")]);
    let mut write = store.begin_write().unwrap();
    write.put(Table::Nodes, b"a", b"2").unwrap();
    let read = store.begin_read().unwrap();
    assert_eq!(read.get(Table::Nodes, b"a").unwrap(), Some(b"1".to_vec()));
    write.commit().unwrap();
    assert_eq!(read.get(Table::Nodes, b"a").unwrap(), Some(b"1".to_vec()));
    assert_eq!(latest(&store, Table::Nodes, b"a"), Some(b"2".to_vec()));
}

const RING: u8 = 8;
const RING_COMMITS: u32 = 40;

fn ring_values(read: &impl ReadTransaction) -> Vec<Vec<u8>> {
    (0..RING)
        .map(|slot| read.get(Table::Nodes, &[slot]).unwrap().unwrap())
        .collect()
}

fn write_ring(store: &impl Storage, round: u32) {
    let mut write = store.begin_write().unwrap();
    for slot in 0..RING {
        write
            .put(Table::Nodes, &[slot], &round.to_be_bytes())
            .unwrap();
    }
    write.commit().unwrap();
}

fn a_reader_keeps_its_snapshot_while_another_thread_commits<H: Harness>(harness: &mut H) {
    let store = harness.open();
    write_ring(&store, 0);
    let pinned = store.begin_read().unwrap();
    let done = AtomicBool::new(false);
    thread::scope(|scope| {
        scope.spawn(|| {
            for round in 1..=RING_COMMITS {
                write_ring(&store, round);
            }
            done.store(true, Ordering::Release);
        });
        while !done.load(Ordering::Acquire) {
            assert!(
                ring_values(&pinned)
                    .iter()
                    .all(|value| value == &0u32.to_be_bytes())
            );
        }
    });
    assert_eq!(ring_values(&pinned)[0], 0u32.to_be_bytes());
    assert_eq!(
        ring_values(&store.begin_read().unwrap())[0],
        RING_COMMITS.to_be_bytes()
    );
}

fn a_commit_becomes_visible_at_once_and_whole<H: Harness>(harness: &mut H) {
    let store = harness.open();
    write_ring(&store, 0);
    let done = AtomicBool::new(false);
    thread::scope(|scope| {
        scope.spawn(|| {
            for round in 1..=RING_COMMITS {
                write_ring(&store, round);
            }
            done.store(true, Ordering::Release);
        });
        let mut last = 0u32;
        while !done.load(Ordering::Acquire) {
            let read = store.begin_read().unwrap();
            let values = ring_values(&read);
            assert!(
                values.iter().all(|value| value == &values[0]),
                "torn commit {values:?}"
            );
            let round = u32::from_be_bytes(values[0].as_slice().try_into().unwrap());
            assert!(round >= last, "a later reader saw an older commit");
            last = round;
        }
    });
}

fn a_second_writer_waits_for_the_first<H: Harness>(harness: &mut H) {
    let store = harness.open();
    put_all(&store, Table::Metadata, &[(b"counter", &[0])]);
    let mut first = store.begin_write().unwrap();
    first.put(Table::Metadata, b"counter", &[1]).unwrap();
    let (began, waited) = mpsc::channel();
    thread::scope(|scope| {
        scope.spawn(|| {
            let mut second = store.begin_write().unwrap();
            began.send(()).unwrap();
            let counter = second.get(Table::Metadata, b"counter").unwrap().unwrap();
            second
                .put(Table::Metadata, b"counter", &[counter[0] + 1])
                .unwrap();
            second.commit().unwrap();
        });
        assert_eq!(
            waited.recv_timeout(Duration::from_millis(200)),
            Err(mpsc::RecvTimeoutError::Timeout),
            "a second writer began while the first was open"
        );
        first.commit().unwrap();
        waited.recv().unwrap();
    });
    assert_eq!(latest(&store, Table::Metadata, b"counter"), Some(vec![2]));
}

fn graph_tables() -> impl Iterator<Item = Table> {
    Table::ALL
        .into_iter()
        .filter(|table| *table != Table::ReplicationHistory)
}

fn commits_survive_a_reopen_and_the_rest_does_not<H: Harness>(harness: &mut H) {
    let store = harness.open();
    let large = patterned(70_000, 3);
    let mut write = store.begin_write().unwrap();
    for table in graph_tables() {
        write.put(table, b"kept", table.name().as_bytes()).unwrap();
    }
    write.put(Table::Vectors, b"large", &large).unwrap();
    write.commit().unwrap();
    put_all(&store, Table::Nodes, &[(b"second", b"2")]);
    let mut write = store.begin_write().unwrap();
    write.put(Table::Nodes, b"rolled back", b"x").unwrap();
    write.rollback().unwrap();
    let mut write = store.begin_write().unwrap();
    write.put(Table::Nodes, b"dropped", b"x").unwrap();
    write.delete(Table::Nodes, b"kept").unwrap();
    drop(write);
    drop(store);

    let store = harness.open();
    let read = store.begin_read().unwrap();
    assert_eq!(
        dump(&read, Table::Nodes),
        entries(&[(b"kept", b"nodes"), (b"second", b"2")])
    );
    assert_eq!(read.get(Table::Vectors, b"large").unwrap(), Some(large));
    for table in graph_tables() {
        assert_eq!(
            read.get(table, b"kept").unwrap(),
            Some(table.name().as_bytes().to_vec())
        );
    }
    drop(read);
    put_all(&store, Table::Nodes, &[(b"third", b"3")]);
    drop(store);

    let store = harness.open();
    assert_eq!(latest(&store, Table::Nodes, b"third"), Some(b"3".to_vec()));
    assert_eq!(latest(&store, Table::Nodes, b"second"), Some(b"2".to_vec()));
}
