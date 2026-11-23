use std::collections::BTreeMap;
use std::ops::Bound;

use proptest::collection::vec;
use proptest::prelude::*;

use super::*;
use crate::engine::block_file::BlockSize;
use crate::engine::block_file::FileSystem;
use crate::engine::check::check;
use crate::engine::header::Header;
use crate::engine::layout::ExtentId;
use crate::engine::mem_fs::MemFile;
use crate::engine::mem_fs::MemFs;
use crate::engine::tree::Tree;
use crate::storage::StorageError;
use crate::test_scale;

const TABLE: TableId = TableId(0x100);
const SEED: u64 = 0x5EED;

type Model = BTreeMap<Vec<u8>, Vec<u8>>;

struct Store {
    pages: Arc<Pages<MemFile>>,
    extents: ExtentTable,
    roots: BTreeMap<TableId, Root>,
    catalog: Vec<RunRef>,
    header: Header,
}

impl Store {
    fn new() -> Self {
        let fs = MemFs::default();
        let file = fs.create("db", BlockSize::new(BLOCK).unwrap()).unwrap();
        Self {
            pages: Arc::new(Pages::new(file, SEED, 256)),
            extents: ExtentTable::default(),
            roots: BTreeMap::new(),
            catalog: Vec::new(),
            header: Header {
                generation: 1,
                next_extent: ExtentId(1),
                ..Header::default()
            },
        }
    }

    fn checkpoint(&mut self, changes: &BTreeMap<Vec<u8>, Option<Vec<u8>>>) {
        let lsn = self.header.checkpoint_lsn + 1;
        let generation = self.header.generation + 1;
        let changes: Vec<(Bytes, Slot)> = changes
            .iter()
            .map(|(key, slot)| {
                (
                    Bytes::from(key.as_slice()),
                    slot.as_deref().map(Bytes::from),
                )
            })
            .collect();
        let mut checkpointer =
            Checkpointer::new(&self.pages, self.extents.clone(), lsn, generation);
        let root = checkpointer
            .merge_table(TABLE, self.roots.get(&TABLE).copied(), &changes)
            .unwrap();
        match root {
            Some(root) => self.roots.insert(TABLE, root),
            None => self.roots.remove(&TABLE),
        };
        let (catalog, extents) = checkpointer
            .finish(self.roots.clone(), &self.catalog)
            .unwrap();
        self.catalog = catalog;
        self.extents = extents;
        self.header.checkpoint_lsn = lsn;
        self.header.generation = generation;
    }

    fn tree(&self) -> Tree {
        Tree {
            roots: self.roots.clone(),
            source: Arc::clone(&self.pages) as Arc<dyn PageSource>,
        }
    }

    fn verify(&self, model: &Model) -> Result<(), TestCaseError> {
        let tree = self.tree();
        let report = check(&self.header, &self.extents, &self.catalog, &tree)
            .map_err(|error| TestCaseError::fail(error.to_string()))?;
        prop_assert_eq!(
            report.entries.get(&TABLE).copied().unwrap_or(0),
            model.len() as u64
        );
        let mut scanned = Vec::new();
        tree.scan::<StorageError>(
            TABLE,
            Bound::Unbounded,
            Bound::Unbounded,
            &mut |key, value| {
                scanned.push((key.to_vec(), value.to_vec()));
                Ok(())
            },
        )
        .unwrap();
        prop_assert_eq!(&scanned, &model.clone().into_iter().collect::<Vec<_>>());
        for (key, value) in model.iter().step_by(model.len() / 50 + 1) {
            let found = tree.get(TABLE, key).unwrap();
            prop_assert_eq!(found.as_deref(), Some(value.as_slice()));
            let mut absent = key.clone();
            absent.push(0x7F);
            prop_assert_eq!(
                tree.get(TABLE, &absent).unwrap(),
                model
                    .get(&absent)
                    .map(|value| Bytes::from(value.as_slice()))
            );
        }
        let keys: Vec<&Vec<u8>> = model.keys().collect();
        for window in keys.windows(2).step_by(keys.len() / 20 + 1) {
            let (start, end) = (window[0], window[1]);
            let mut ranged = Vec::new();
            tree.scan::<StorageError>(
                TABLE,
                Bound::Excluded(start),
                Bound::Included(end),
                &mut |key, _| {
                    ranged.push(key.to_vec());
                    Ok(())
                },
            )
            .unwrap();
            prop_assert_eq!(ranged, vec![end.clone()]);
        }
        Ok(())
    }
}

fn apply(model: &mut Model, changes: &BTreeMap<Vec<u8>, Option<Vec<u8>>>) {
    for (key, slot) in changes {
        match slot {
            Some(value) => model.insert(key.clone(), value.clone()),
            None => model.remove(key),
        };
    }
}

fn value(length: usize, seed: u8) -> Vec<u8> {
    (0..length)
        .map(|index| seed.wrapping_add(index as u8))
        .collect()
}

#[test]
fn sequential_appends_fill_pages_and_grow_the_tree() {
    let mut store = Store::new();
    let mut model = Model::new();
    for batch in 0..8u64 {
        let changes: BTreeMap<_, _> = (batch * 4000..(batch + 1) * 4000)
            .map(|id| (id.to_be_bytes().to_vec(), Some(value(12, id as u8))))
            .collect();
        apply(&mut model, &changes);
        store.checkpoint(&changes);
        store.verify(&model).unwrap();
    }
    assert!(store.roots[&TABLE].level >= 1);
    let report = check(&store.header, &store.extents, &store.catalog, &store.tree()).unwrap();
    let leaves_needed = (32_000 * 18) / (BLOCK - 64);
    assert!(
        (report.pages as usize) < leaves_needed * 11 / 10 + 40,
        "appends left half-empty pages: {} pages for about {leaves_needed} leaves",
        report.pages
    );
}

#[test]
fn deleting_almost_everything_shrinks_the_tree_and_frees_its_blocks() {
    let mut store = Store::new();
    let mut model = Model::new();
    let changes: BTreeMap<_, _> = (0..20_000u64)
        .map(|id| {
            (
                id.to_be_bytes().to_vec(),
                Some(value((id % 50) as usize, id as u8)),
            )
        })
        .collect();
    apply(&mut model, &changes);
    store.checkpoint(&changes);
    let full = check(&store.header, &store.extents, &store.catalog, &store.tree()).unwrap();
    let doomed: BTreeMap<_, _> = (0..20_000u64)
        .filter(|id| id % 1000 != 7)
        .map(|id| (id.to_be_bytes().to_vec(), None))
        .collect();
    apply(&mut model, &doomed);
    store.checkpoint(&doomed);
    store.verify(&model).unwrap();
    let small = check(&store.header, &store.extents, &store.catalog, &store.tree()).unwrap();
    assert!(store.roots[&TABLE].level <= 1);
    assert!(small.live_blocks < 10, "{small:?} after {full:?}");
    let everything: BTreeMap<_, _> = model.keys().map(|key| (key.clone(), None)).collect();
    store.checkpoint(&everything);
    assert!(store.roots.is_empty());
    store.verify(&Model::new()).unwrap();
}

#[test]
fn long_keys_and_values_live_in_runs_that_are_freed_with_them() {
    let mut store = Store::new();
    let mut model = Model::new();
    let shared = vec![b'k'; 3000];
    let mut changes = BTreeMap::new();
    for index in 0..60u32 {
        let mut key = shared.clone();
        key.extend_from_slice(&index.to_be_bytes());
        key.extend(std::iter::repeat_n(b'z', (index % 3) as usize * 700));
        let length = [0, 1024, 1025, 9000, 5_000_000][index as usize % 5];
        changes.insert(key, Some(value(length, index as u8)));
    }
    apply(&mut model, &changes);
    store.checkpoint(&changes);
    store.verify(&model).unwrap();
    let removed: BTreeMap<_, _> = model
        .keys()
        .step_by(2)
        .map(|key| (key.clone(), None))
        .collect();
    apply(&mut model, &removed);
    store.checkpoint(&removed);
    store.verify(&model).unwrap();
    let rewritten: BTreeMap<_, _> = model
        .keys()
        .map(|key| (key.clone(), Some(b"short".to_vec())))
        .collect();
    apply(&mut model, &rewritten);
    store.checkpoint(&rewritten);
    store.verify(&model).unwrap();
}

#[test]
fn long_separators_make_a_deep_tree_that_still_reads_back() {
    let mut store = Store::new();
    let mut model = Model::new();
    let changes: BTreeMap<_, _> = (0..600u32)
        .map(|index| {
            let mut key = vec![b's'; 1000];
            key.extend_from_slice(&index.to_be_bytes());
            (key, Some(value(5, index as u8)))
        })
        .collect();
    apply(&mut model, &changes);
    store.checkpoint(&changes);
    store.verify(&model).unwrap();
    assert!(store.roots[&TABLE].level >= 3, "{:?}", store.roots[&TABLE]);
    let thinned: BTreeMap<_, _> = model
        .keys()
        .filter(|key| key[1003] % 5 != 0)
        .map(|key| (key.clone(), None))
        .collect();
    apply(&mut model, &thinned);
    store.checkpoint(&thinned);
    store.verify(&model).unwrap();
}

#[test]
fn a_single_key_far_longer_than_a_page_round_trips() {
    let mut store = Store::new();
    let mut model = Model::new();
    let changes: BTreeMap<_, _> = [
        (vec![1u8; 100_000], Some(b"a".to_vec())),
        (vec![1u8; 100_001], Some(vec![9; 2000])),
        (vec![0u8], Some(b"b".to_vec())),
    ]
    .into_iter()
    .collect();
    apply(&mut model, &changes);
    store.checkpoint(&changes);
    store.verify(&model).unwrap();
}

#[derive(Clone, Debug)]
enum Keys {
    Random,
    Appending,
    Long,
}

fn key(keys: &Keys, step: usize, pick: u16) -> Vec<u8> {
    match keys {
        Keys::Random => (u64::from(pick) * 2_654_435_761 % 5000)
            .to_be_bytes()
            .to_vec(),
        Keys::Appending => ((step as u64) << 16 | u64::from(pick % 256))
            .to_be_bytes()
            .to_vec(),
        Keys::Long => {
            let mut key = vec![b'p'; 900 + usize::from(pick % 5) * 100];
            key.extend_from_slice(&(pick % 400).to_be_bytes());
            key
        }
    }
}

type Batch = Vec<(u16, Option<u16>)>;

fn batches() -> impl Strategy<Value = (Keys, Vec<Batch>)> {
    (
        prop_oneof![Just(Keys::Random), Just(Keys::Appending), Just(Keys::Long)],
        vec(
            vec(
                (any::<u16>(), proptest::option::weighted(0.75, any::<u16>())),
                0..300,
            ),
            1..8,
        ),
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(test_scale::cases(24)))]

    #[test]
    fn checkpoints_of_random_changes_match_a_map((keys, batches) in batches()) {
        let mut store = Store::new();
        let mut model = Model::new();
        for (step, batch) in batches.iter().enumerate() {
            let changes: BTreeMap<_, _> = batch
                .iter()
                .map(|(pick, length)| {
                    let length = length.map(|length| match length % 16 {
                        0 => 1500,
                        1 => 1024,
                        2 => 1025,
                        _ => usize::from(length % 60),
                    });
                    (key(&keys, step, *pick), length.map(|length| value(length, *pick as u8)))
                })
                .collect();
            apply(&mut model, &changes);
            store.checkpoint(&changes);
            store.verify(&model)?;
        }
    }
}
