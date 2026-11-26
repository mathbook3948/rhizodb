use std::collections::BTreeMap;
use std::ops::Bound;
use std::ops::RangeBounds;

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
use crate::engine::tree::Finger;
use crate::engine::tree::Tree;
use crate::storage::StorageError;
use crate::test_scale;

const TABLE: TableId = TableId(0x100);
const SEED: u64 = 0x5EED;

type Model = BTreeMap<Vec<u8>, Vec<u8>>;

struct Store {
    fs: MemFs,
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
            fs,
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
        let finished = checkpointer
            .finish(self.roots.clone(), &self.catalog)
            .unwrap();
        self.catalog = finished.catalog;
        self.extents = finished.extents;
        self.header.checkpoint_lsn = lsn;
        self.header.generation = generation;
    }

    fn pages_file(&self) -> MemFile {
        self.fs.open("db", BlockSize::new(BLOCK).unwrap()).unwrap()
    }

    fn tree(&self) -> Tree {
        Tree::new(
            self.header.generation,
            self.roots.clone(),
            Arc::clone(&self.pages) as Arc<dyn PageSource>,
        )
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

fn scan_bound(keys: &[Vec<u8>], kind: u8, pick: usize, between: bool) -> Bound<Vec<u8>> {
    let mut key = keys[pick % keys.len()].clone();
    if between {
        key.push(0);
    }
    match kind {
        0 => Bound::Unbounded,
        1 => Bound::Included(key),
        _ => Bound::Excluded(key),
    }
}

type Scan = (u8, u16, bool, u8, u16, bool);

fn deep_store(long_keys: bool) -> (Store, Model) {
    let mut store = Store::new();
    let mut model = Model::new();
    let changes: BTreeMap<_, _> = (0..3000u32)
        .map(|index| {
            let mut key = (index * 2).to_be_bytes().to_vec();
            if long_keys && index % 97 == 0 {
                key.extend(std::iter::repeat_n(b'x', 1100));
            }
            (key, Some(value((index % 7) as usize * 150, index as u8)))
        })
        .collect();
    apply(&mut model, &changes);
    store.checkpoint(&changes);
    (store, model)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(test_scale::cases(24)))]

    #[test]
    fn scans_that_start_near_the_last_one_find_what_the_map_holds(
        long_keys in any::<bool>(),
        scans in vec((0u8..3, any::<u16>(), any::<bool>(), 0u8..3, 0u16..40, any::<bool>()), 1..80),
    ) {
        let (store, model) = deep_store(long_keys);
        prop_assert!(store.roots[&TABLE].level >= 2, "{:?}", store.roots[&TABLE]);
        let tree = store.tree();
        let keys: Vec<Vec<u8>> = model.keys().cloned().collect();
        let finger = Finger::default();
        let mut pick = 0usize;
        for (start_kind, step, start_between, end_kind, width, end_between) in scans {
            let scan: Scan = (start_kind, step, start_between, end_kind, width, end_between);
            pick = pick.wrapping_add(usize::from(step % 64));
            if step % 9 == 0 {
                pick = usize::from(step);
            }
            let start = scan_bound(&keys, start_kind, pick, start_between);
            let end = scan_bound(&keys, end_kind, pick + usize::from(width), end_between);
            let expected: Vec<(Vec<u8>, Vec<u8>)> = model
                .iter()
                .filter(|(key, _)| (start.clone(), end.clone()).contains(*key))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            let mut scanned = Vec::new();
            tree.scan_near::<StorageError>(
                TABLE,
                start.as_ref().map(Vec::as_slice),
                end.as_ref().map(Vec::as_slice),
                &mut |key, value| {
                    scanned.push((key.to_vec(), value.to_vec()));
                    Ok(())
                },
                &finger,
            )
            .unwrap();
            prop_assert_eq!(scanned, expected, "{:?}", scan);
        }
    }

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

#[test]
fn deleting_all_but_two_keys_of_a_deep_tree_leaves_no_chain_of_single_children() {
    let mut store = Store::new();
    let mut model = Model::new();
    let key = |index: u64| [vec![b'p'; 900], index.to_be_bytes().to_vec()].concat();
    let changes: BTreeMap<_, _> = (0..20_000)
        .map(|index| (key(index), Some(b"v".to_vec())))
        .collect();
    apply(&mut model, &changes);
    store.checkpoint(&changes);
    assert!(store.roots[&TABLE].level >= 3);
    let doomed: BTreeMap<_, _> = (0..19_998).map(|index| (key(index), None)).collect();
    apply(&mut model, &doomed);
    store.checkpoint(&doomed);
    store.verify(&model).unwrap();
    let root = store.roots[&TABLE];
    if root.level > 0 {
        let cached =
            crate::engine::tree::fetch(&*store.pages, TABLE, root.page, root.level).unwrap();
        assert!(
            cached.header.cells > 1,
            "the root {root:?} is a branch with one child"
        );
    }
}

#[test]
fn a_catalog_longer_than_an_extent_reads_back_from_the_header_alone() {
    let mut store = Store::new();
    for index in 0..120_000u64 {
        store.extents.entries.insert(
            ExtentId(1_000_000 + index),
            crate::engine::extent::ExtentEntry {
                slot: 50_000_000 + index,
                written: 1,
                used: crate::engine::extent::Blocks::range(0, 1),
                state: crate::engine::extent::ExtentState::Sealed,
                purpose: crate::engine::extent::Purpose::Tree,
                scheme: 0,
                opened_term: 0,
            },
        );
    }
    store.extents.next_extent = ExtentId(2_000_000);
    let changes: BTreeMap<_, _> = [(b"k".to_vec(), Some(b"v".to_vec()))].into_iter().collect();
    store.checkpoint(&changes);
    assert!(store.catalog.len() >= 2, "{:?}", store.catalog);
    assert!(
        store
            .catalog
            .iter()
            .map(|run| run.at.extent())
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            >= 2
    );
    let fresh: Arc<Pages<MemFile>> = Arc::new(Pages::new(store.pages_file(), SEED, 16));
    for open in store.extents.open_extents().unwrap() {
        if !open.id.is_none() {
            fresh.place_extent(open.id, open.slot);
        }
    }
    let source: Arc<dyn PageSource> = Arc::clone(&fresh) as Arc<dyn PageSource>;
    let (catalog, runs) = load_catalog(&source, store.catalog[0], &|id, slot| {
        fresh.place_extent(id, slot)
    })
    .unwrap();
    assert_eq!(runs, store.catalog);
    assert_eq!(catalog.extents, store.extents);
    assert_eq!(catalog.roots, store.roots);
}
