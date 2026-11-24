use std::sync::Arc;

use proptest::collection::vec;
use proptest::prelude::*;

use crate::engine::sim_fs::SimFs;
use crate::engine::store::CheckpointMode;
use crate::engine::store::Engine;
use crate::engine::store::EngineOptions;
use crate::storage::ReadTransaction;
use crate::storage::Storage;
use crate::storage::Table;
use crate::storage::WriteTransaction;
use crate::storage::redb_backend::RedbStorage;
use crate::test_scale;

const TABLES: [Table; 3] = [Table::Adjacency, Table::NodeColumns, Table::KeywordPostings];

#[derive(Clone, Debug)]
enum Step {
    Put(usize, Vec<u8>, usize),
    Delete(usize, Vec<u8>),
    DeleteRange(usize, Vec<u8>, Vec<u8>),
    Commit,
    Rollback,
    Get(usize, Vec<u8>),
    Prefix(usize, Vec<u8>),
    Range(usize, Vec<u8>, Vec<u8>),
    Reopen,
}

fn key() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        6 => (0u32..3000).prop_map(|id| id.to_be_bytes().to_vec()),
        2 => vec(0u8..3, 0..4),
        1 => (0usize..3, 0u8..4).prop_map(|(shape, last)| {
            let mut key = vec![0xAB; [1100, 3000, 20_000][shape]];
            key.push(last);
            key
        }),
    ]
}

fn length() -> impl Strategy<Value = usize> {
    prop_oneof![10 => 0usize..64, 1 => Just(1024), 1 => Just(1025), 1 => 2000usize..20_000, 1 => Just(300_000)]
}

fn step() -> impl Strategy<Value = Step> {
    let table = 0..TABLES.len();
    prop_oneof![
        20 => (table.clone(), key(), length()).prop_map(|(table, key, length)| Step::Put(table, key, length)),
        4 => (table.clone(), key()).prop_map(|(table, key)| Step::Delete(table, key)),
        1 => (table.clone(), key(), key()).prop_map(|(table, start, end)| Step::DeleteRange(table, start, end)),
        6 => Just(Step::Commit),
        1 => Just(Step::Rollback),
        4 => (table.clone(), key()).prop_map(|(table, key)| Step::Get(table, key)),
        2 => (table.clone(), vec(0u8..3, 0..3)).prop_map(|(table, prefix)| Step::Prefix(table, prefix)),
        2 => (table, key(), key()).prop_map(|(table, start, end)| Step::Range(table, start, end)),
        1 => Just(Step::Reopen),
    ]
}

fn compare(steps: &[Step]) -> Result<(), TestCaseError> {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("reference.redb");
    let fs = SimFs::new();
    let options = EngineOptions {
        segment_target: 64 << 10,
        memtable_limit: 256 << 10,
        wal_limit: 1 << 20,
        checkpoint: CheckpointMode::Inline,
        cache_pages: 64,
    };
    let mut reference = Some(RedbStorage::open(&path).unwrap());
    let mut engine = Some(Engine::open(Arc::new(fs.clone()), "graph.rhizo", options).unwrap());
    let mut stamp = 0u32;
    let mut index = 0;
    while index < steps.len() {
        let (redb, ours) = (reference.as_ref().unwrap(), engine.as_ref().unwrap());
        let mut left = redb.begin_write().unwrap();
        let mut right = ours.begin_write().unwrap();
        while index < steps.len() {
            let step = &steps[index];
            index += 1;
            match step {
                Step::Put(table, key, length) => {
                    stamp += 1;
                    let value: Vec<u8> = (0..*length)
                        .map(|at| (stamp as usize + at / 3) as u8)
                        .collect();
                    left.put(TABLES[*table], key, &value).unwrap();
                    right.put(TABLES[*table], key, &value).unwrap();
                }
                Step::Delete(table, key) => {
                    left.delete(TABLES[*table], key).unwrap();
                    right.delete(TABLES[*table], key).unwrap();
                }
                Step::DeleteRange(table, start, end) => {
                    left.delete_range(TABLES[*table], start, end).unwrap();
                    right.delete_range(TABLES[*table], start, end).unwrap();
                }
                Step::Get(table, key) => {
                    prop_assert_eq!(
                        left.get(TABLES[*table], key).unwrap(),
                        right.get(TABLES[*table], key).unwrap()
                    );
                }
                Step::Prefix(table, prefix) => {
                    prop_assert_eq!(
                        left.scan_prefix(TABLES[*table], prefix).unwrap(),
                        right.scan_prefix(TABLES[*table], prefix).unwrap()
                    );
                }
                Step::Range(table, start, end) => {
                    prop_assert_eq!(
                        left.scan_range(TABLES[*table], start, end).unwrap(),
                        right.scan_range(TABLES[*table], start, end).unwrap()
                    );
                }
                Step::Commit => {
                    left.commit().unwrap();
                    right.commit().unwrap();
                    break;
                }
                Step::Rollback => {
                    left.rollback().unwrap();
                    right.rollback().unwrap();
                    break;
                }
                Step::Reopen => {
                    left.commit().unwrap();
                    right.commit().unwrap();
                    drop(reference.take());
                    drop(engine.take());
                    reference = Some(RedbStorage::open(&path).unwrap());
                    engine =
                        Some(Engine::open(Arc::new(fs.clone()), "graph.rhizo", options).unwrap());
                    break;
                }
            }
        }
        let (redb, ours) = (reference.as_ref().unwrap(), engine.as_ref().unwrap());
        let (left, right) = (redb.begin_read().unwrap(), ours.begin_read().unwrap());
        for table in TABLES {
            prop_assert_eq!(
                left.scan_prefix(table, &[]).unwrap(),
                right.scan_prefix(table, &[]).unwrap()
            );
        }
    }
    engine
        .as_ref()
        .unwrap()
        .check()
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(test_scale::cases(32)))]

    #[test]
    fn the_engine_returns_the_bytes_redb_returns(steps in vec(step(), 1..400)) {
        compare(&steps)?;
    }
}

#[test]
fn thousands_of_keys_in_many_commits_match_redb() {
    let mut steps = Vec::new();
    for round in 0..40u32 {
        for item in 0..150u32 {
            let id = (round * 7919 + item * 104_729) % 6000;
            steps.push(Step::Put(
                (item % 3) as usize,
                id.to_be_bytes().to_vec(),
                (item % 5) as usize * 300,
            ));
        }
        steps.push(Step::DeleteRange(
            0,
            (round * 100).to_be_bytes().to_vec(),
            (round * 100 + 37).to_be_bytes().to_vec(),
        ));
        steps.push(Step::Commit);
        if round % 13 == 12 {
            steps.push(Step::Reopen);
        }
    }
    compare(&steps).unwrap();
}
