use super::*;
use crate::engine::sim_fs::SimFs;
use crate::storage::conformance;
use crate::storage::conformance::Harness;
use crate::storage::model;
use crate::storage::sim_backend;
use crate::test_scale;
use proptest::prelude::*;

struct RedbFile {
    directory: tempfile::TempDir,
}

impl RedbFile {
    fn new() -> Self {
        Self {
            directory: tempfile::tempdir().unwrap(),
        }
    }
}

impl Harness for RedbFile {
    type Store = RedbStorage;

    fn open(&mut self) -> RedbStorage {
        RedbStorage::open(self.directory.path().join("graph.redb")).unwrap()
    }
}

struct RedbOnSimFs {
    fs: SimFs,
}

impl Harness for RedbOnSimFs {
    type Store = RedbStorage;

    fn open(&mut self) -> RedbStorage {
        sim_backend::storage(&self.fs).unwrap()
    }
}

fn on_sim_fs() -> RedbOnSimFs {
    RedbOnSimFs { fs: SimFs::new() }
}

#[test]
fn the_redb_backend_passes_the_storage_conformance_suite() {
    conformance::run(RedbFile::new);
}

#[test]
fn the_redb_backend_on_the_simulated_file_system_passes_the_storage_conformance_suite() {
    conformance::run(on_sim_fs);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(test_scale::cases(128)))]

    #[test]
    fn the_redb_backend_matches_the_versioned_model(ops in model::ops()) {
        model::check(&mut RedbFile::new(), &ops)?;
    }

    #[test]
    fn the_redb_backend_on_the_simulated_file_system_matches_the_versioned_model(ops in model::ops()) {
        model::check(&mut on_sim_fs(), &ops)?;
    }
}

fn put_all(storage: &RedbStorage, table: Table, entries: &[(&[u8], &[u8])]) {
    let mut write = storage.begin_write().unwrap();
    for (key, value) in entries {
        write.put(table, key, value).unwrap();
    }
    write.commit().unwrap();
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
