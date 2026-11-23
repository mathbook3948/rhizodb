use super::*;
use proptest::prelude::*;

fn put_all(storage: &RedbStorage, table: Table, entries: &[(&[u8], &[u8])]) {
    let mut write = storage.begin_write().unwrap();
    for (key, value) in entries {
        write.put(table, key, value).unwrap();
    }
    write.commit().unwrap();
}

#[test]
fn committed_put_is_readable_and_delete_removes_it() {
    let storage = RedbStorage::in_memory().unwrap();
    put_all(&storage, Table::Nodes, &[(b"a", b"1")]);

    let read = storage.begin_read().unwrap();
    assert_eq!(read.get(Table::Nodes, b"a").unwrap(), Some(b"1".to_vec()));
    assert_eq!(read.get(Table::Nodes, b"missing").unwrap(), None);
    assert_eq!(read.get(Table::Relationships, b"a").unwrap(), None);

    let mut write = storage.begin_write().unwrap();
    write.delete(Table::Nodes, b"a").unwrap();
    write.commit().unwrap();
    assert_eq!(
        storage
            .begin_read()
            .unwrap()
            .get(Table::Nodes, b"a")
            .unwrap(),
        None
    );
}

#[test]
fn dropping_a_write_transaction_discards_its_writes() {
    let storage = RedbStorage::in_memory().unwrap();
    {
        let mut write = storage.begin_write().unwrap();
        write.put(Table::Nodes, b"a", b"1").unwrap();
        assert_eq!(write.get(Table::Nodes, b"a").unwrap(), Some(b"1".to_vec()));
    }

    assert_eq!(
        storage
            .begin_read()
            .unwrap()
            .get(Table::Nodes, b"a")
            .unwrap(),
        None
    );
}

#[test]
fn a_rolled_back_write_transaction_discards_its_writes() {
    let storage = RedbStorage::in_memory().unwrap();
    let mut write = storage.begin_write().unwrap();
    write.put(Table::Nodes, b"a", b"1").unwrap();
    write.rollback().unwrap();

    assert_eq!(
        storage
            .begin_read()
            .unwrap()
            .get(Table::Nodes, b"a")
            .unwrap(),
        None
    );
}

#[test]
fn prefix_scan_returns_only_matching_keys_in_ascending_order() {
    let storage = RedbStorage::in_memory().unwrap();
    put_all(
        &storage,
        Table::Outgoing,
        &[
            (&[2, 0], b"x"),
            (&[1, 9], b"b"),
            (&[1, 1], b"a"),
            (&[0, 5], b"y"),
            (&[1], b"c"),
        ],
    );

    let read = storage.begin_read().unwrap();
    let keys: Vec<Vec<u8>> = read
        .scan_prefix(Table::Outgoing, &[1])
        .unwrap()
        .into_iter()
        .map(|(key, _)| key)
        .collect();
    assert_eq!(keys, vec![vec![1], vec![1, 1], vec![1, 9]]);
    assert_eq!(read.scan_prefix(Table::Outgoing, &[]).unwrap().len(), 5);
    assert!(read.scan_prefix(Table::Outgoing, &[3]).unwrap().is_empty());
}

#[test]
fn a_read_transaction_keeps_its_snapshot_while_a_writer_commits() {
    let storage = RedbStorage::in_memory().unwrap();
    put_all(&storage, Table::Nodes, &[(b"a", b"1")]);

    let read = storage.begin_read().unwrap();
    put_all(&storage, Table::Nodes, &[(b"a", b"2")]);

    assert_eq!(read.get(Table::Nodes, b"a").unwrap(), Some(b"1".to_vec()));
    assert_eq!(
        storage
            .begin_read()
            .unwrap()
            .get(Table::Nodes, b"a")
            .unwrap(),
        Some(b"2".to_vec())
    );
}

#[test]
fn a_file_database_keeps_committed_data_across_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph.redb");
    {
        let storage = RedbStorage::open(&path).unwrap();
        put_all(&storage, Table::Nodes, &[(b"a", b"1")]);
    }

    let reopened = RedbStorage::open(&path).unwrap();
    assert_eq!(
        reopened
            .begin_read()
            .unwrap()
            .get(Table::Nodes, b"a")
            .unwrap(),
        Some(b"1".to_vec())
    );
}

#[test]
fn a_range_scan_returns_the_keys_from_its_start_up_to_its_end() {
    let storage = RedbStorage::in_memory().unwrap();
    put_all(
        &storage,
        Table::PropertyEntries,
        &[(b"a", b"1"), (b"b", b"2"), (b"ba", b"3"), (b"c", b"4")],
    );
    let read = storage.begin_read().unwrap();
    let keys = |start: &[u8], end: &[u8]| -> Vec<Vec<u8>> {
        read.scan_range(Table::PropertyEntries, start, end)
            .unwrap()
            .into_iter()
            .map(|(key, _)| key)
            .collect()
    };
    assert_eq!(keys(b"b", b"c"), vec![b"b".to_vec(), b"ba".to_vec()]);
    assert_eq!(keys(b"a", b"b"), vec![b"a".to_vec()]);
    assert_eq!(keys(b"c", b"b"), Vec::<Vec<u8>>::new());
    let write = storage.begin_write().unwrap();
    assert_eq!(
        write
            .scan_range(Table::PropertyEntries, b"", b"z")
            .unwrap()
            .len(),
        4
    );
}

fn reach() -> impl Strategy<Value = Reach> {
    prop_oneof![Just(Reach::Step), Just(Reach::Seek), Just(Reach::Lookup)]
}

proptest! {
    #[test]
    fn a_cursor_reads_what_a_lookup_reads_whatever_the_order_and_reach(
        stored in prop::collection::btree_set(0u64..64, 0..40),
        reads in prop::collection::vec((0u64..72, reach()), 0..60),
    ) {
        let storage = RedbStorage::in_memory().unwrap();
        let entries: Vec<([u8; 8], [u8; 9])> = stored
            .iter()
            .map(|key| {
                let mut value = [7u8; 9];
                value[1..].copy_from_slice(&key.to_le_bytes());
                (key.to_be_bytes(), value)
            })
            .collect();
        let borrowed: Vec<(&[u8], &[u8])> = entries
            .iter()
            .map(|(key, value)| (key.as_slice(), value.as_slice()))
            .collect();
        put_all(&storage, Table::Nodes, &borrowed);
        let read = storage.begin_read().unwrap();
        let mut cursor = read.cursor(Table::Nodes).unwrap();
        for (key, reach) in reads {
            let key = key.to_be_bytes();
            let found = cursor
                .get_with(&key, reach, |value| Ok::<_, StorageError>(value.to_vec()))
                .unwrap();
            prop_assert_eq!(found, read.get(Table::Nodes, &key).unwrap());
        }
    }
}
