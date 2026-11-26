use super::*;
use crate::engine::layout::ExtentId;
use crate::engine::layout::physical_block;
use crate::engine::mem_fs::MemFs;
use crate::engine::sim_fs::Crash;
use crate::engine::sim_fs::SimFs;
use crate::storage::conformance;
use crate::storage::conformance::Harness;
use crate::storage::model;
use crate::test_scale;
use proptest::prelude::*;

const NAME: &str = "graph.rhizo";

fn tiny(checkpoint: CheckpointMode) -> EngineOptions {
    EngineOptions {
        segment_target: 4096,
        memtable_limit: 2048,
        wal_limit: 16_384,
        checkpoint,
        cache_pages: 32,
        reclaim_slack: 64 << 10,
        stage_floor: 8,
    }
}

struct OnSimFs {
    fs: SimFs,
    options: EngineOptions,
}

impl Harness for OnSimFs {
    type Store = Engine<SimFs>;

    fn open(&mut self) -> Engine<SimFs> {
        let engine = Engine::open(Arc::new(self.fs.clone()), NAME, self.options).unwrap();
        engine.check().unwrap();
        engine
    }
}

struct InMemory {
    fs: Arc<MemFs>,
}

impl Harness for InMemory {
    type Store = Engine<MemFs>;

    fn open(&mut self) -> Engine<MemFs> {
        Engine::open(Arc::clone(&self.fs), NAME, EngineOptions::default()).unwrap()
    }
}

fn on_sim_fs(options: EngineOptions) -> impl FnMut() -> OnSimFs {
    move || OnSimFs {
        fs: SimFs::new(),
        options,
    }
}

#[test]
fn the_engine_passes_the_storage_conformance_suite() {
    conformance::run(on_sim_fs(EngineOptions::default()));
}

#[test]
fn the_engine_passes_the_conformance_suite_checkpointing_inline_all_the_time() {
    conformance::run(on_sim_fs(tiny(CheckpointMode::Inline)));
}

#[test]
fn the_engine_passes_the_conformance_suite_checkpointing_in_the_background_all_the_time() {
    conformance::run(on_sim_fs(tiny(CheckpointMode::Background)));
}

#[test]
fn the_engine_in_memory_passes_the_storage_conformance_suite() {
    conformance::run(|| InMemory {
        fs: Arc::new(MemFs::default()),
    });
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(test_scale::cases(96)))]

    #[test]
    fn the_engine_matches_the_versioned_model(ops in model::ops()) {
        model::check(&mut on_sim_fs(EngineOptions::default())(), &ops)?;
    }

    #[test]
    fn the_engine_checkpointing_inline_matches_the_versioned_model(ops in model::ops()) {
        model::check(&mut on_sim_fs(tiny(CheckpointMode::Inline))(), &ops)?;
    }

    #[test]
    fn the_engine_checkpointing_in_the_background_matches_the_versioned_model(ops in model::ops()) {
        model::check(&mut on_sim_fs(tiny(CheckpointMode::Background))(), &ops)?;
    }
}

fn fill(engine: &impl Storage, rounds: u32) {
    for round in 0..rounds {
        let mut write = engine.begin_write().unwrap();
        for item in 0..20u32 {
            let key = (round * 20 + item).to_be_bytes();
            let length = [3, 40, 1500, 9000][(item % 4) as usize];
            write
                .put(Table::Nodes, &key, &vec![round as u8; length])
                .unwrap();
        }
        write
            .delete(Table::Nodes, &(round * 7).to_be_bytes())
            .unwrap();
        write.commit().unwrap();
    }
}

fn everything(engine: &impl Storage) -> Result<Vec<Entry>, StorageError> {
    let read = engine.begin_read()?;
    let mut all = Vec::new();
    for table in Table::ALL {
        let entries = read.scan_prefix(table, &[])?;
        for (key, value) in &entries {
            if read.get(table, key)?.as_ref() != Some(value) {
                return Err(StorageError::new(EngineError::corrupt(
                    "a get differs from the scan",
                )));
            }
        }
        all.extend(entries);
    }
    Ok(all)
}

#[test]
fn a_clean_close_leaves_one_file_that_opens_without_a_log() {
    let fs = SimFs::new();
    let engine = Engine::open(Arc::new(fs.clone()), NAME, tiny(CheckpointMode::Inline)).unwrap();
    fill(&engine, 30);
    let expected = everything(&engine).unwrap();
    drop(engine);
    assert_eq!(fs.names().unwrap(), [NAME]);
    let crashed = fs.crash(Crash::DropUnsynced);
    let engine = Engine::open(Arc::new(crashed), NAME, EngineOptions::default()).unwrap();
    assert!(engine.header().generation > 3);
    assert_eq!(everything(&engine).unwrap(), expected);
    engine.check().unwrap();
}

#[test]
fn an_unclean_file_without_its_log_is_refused() {
    let fs = SimFs::new();
    let engine = Engine::open(Arc::new(fs.clone()), NAME, EngineOptions::default()).unwrap();
    fill(&engine, 2);
    let crashed = fs.crash(Crash::DropUnsynced);
    drop(engine);
    for name in crashed.names().unwrap() {
        if name != NAME {
            crashed.remove(&name).unwrap();
        }
    }
    let error = Engine::open(Arc::new(crashed), NAME, EngineOptions::default()).unwrap_err();
    assert!(matches!(error, EngineError::WalMissing), "{error}");
}

#[test]
fn an_explicit_checkpoint_drops_the_log_and_keeps_every_commit() {
    let fs = SimFs::new();
    let engine = Engine::open(
        Arc::new(fs.clone()),
        NAME,
        EngineOptions {
            segment_target: 4096,
            ..EngineOptions::default()
        },
    )
    .unwrap();
    fill(&engine, 20);
    let before = segment_files(&fs, NAME).unwrap().len();
    assert!(before > 3);
    engine.checkpoint().unwrap();
    fill(&engine, 1);
    assert!(segment_files(&fs, NAME).unwrap().len() < before);
    let expected = everything(&engine).unwrap();
    let crashed = fs.crash(Crash::DropUnsynced);
    drop(engine);
    let engine = Engine::open(Arc::new(crashed), NAME, EngineOptions::default()).unwrap();
    assert_eq!(everything(&engine).unwrap(), expected);
    engine.check().unwrap();
}

type LiveBlocks = std::collections::BTreeMap<(ExtentId, u32), Vec<u8>>;

fn live_blocks(fs: &SimFs, extents: &ExtentTable) -> LiveBlocks {
    let file = fs.open(NAME, block_size().unwrap()).unwrap();
    let mut blocks = LiveBlocks::new();
    for (id, entry) in &extents.entries {
        for (start, length) in entry.used.ranges() {
            for block in start..start + length {
                let at = physical_block(entry.slot, block).unwrap();
                let mut bytes = vec![0; BLOCK];
                file.read(at, &mut bytes).unwrap();
                blocks.insert((*id, block), bytes);
            }
        }
    }
    blocks
}

fn manual(checkpoint: CheckpointMode) -> EngineOptions {
    EngineOptions {
        memtable_limit: u64::MAX,
        wal_limit: u64::MAX,
        ..tiny(checkpoint)
    }
}

#[test]
fn a_block_the_durable_header_reaches_never_changes_and_freed_blocks_take_later_pages() {
    let fs = SimFs::new();
    let unstaged = EngineOptions {
        stage_floor: u64::MAX,
        ..manual(CheckpointMode::Inline)
    };
    let engine = Engine::open(Arc::new(fs.clone()), NAME, unstaged).unwrap();
    let mut previous: Option<LiveBlocks> = None;
    let mut freed = std::collections::BTreeSet::new();
    let mut checked = 0;
    let mut reused = 0;
    for round in 0..40u32 {
        let mut write = engine.begin_write().unwrap();
        for item in 0..200u32 {
            let key = (item * 7 + round) % 600;
            let length = 30 + (item % 3) as usize * 600;
            write
                .put(Table::Nodes, &key.to_be_bytes(), &vec![round as u8; length])
                .unwrap();
        }
        write.commit().unwrap();
        engine.checkpoint().unwrap();
        engine.check().unwrap();
        let extents = engine.extents();
        let now = live_blocks(&fs, &extents);
        if let Some(before) = previous.take() {
            for (place, bytes) in &before {
                match now.get(place) {
                    Some(after) if after == bytes => checked += 1,
                    Some(_) => panic!("block {place:?} changed while a durable header reached it"),
                    None => {
                        freed.insert(*place);
                    }
                }
            }
            reused += now
                .keys()
                .filter(|place| !before.contains_key(place) && freed.contains(place))
                .count();
        }
        previous = Some(now);
    }
    assert!(checked > 1000);
    assert!(
        reused > 1000,
        "only {reused} freed blocks took a later page"
    );
}

#[test]
fn every_damaged_byte_fails_as_corruption_or_reads_as_before() {
    let fs = SimFs::new();
    let engine = Engine::open(Arc::new(fs.clone()), NAME, tiny(CheckpointMode::Inline)).unwrap();
    fill(&engine, 8);
    let expected = everything(&engine).unwrap();
    drop(engine);
    let pristine = fs.crash(Crash::DropUnsynced);
    let blocks = pristine
        .open(NAME, block_size().unwrap())
        .unwrap()
        .block_count()
        .unwrap();
    let stride = (blocks * BLOCK as u64 / u64::from(test_scale::cases(400))).max(1);
    let mut outcomes = [0usize; 2];
    let mut offset = 0;
    while offset < blocks * BLOCK as u64 {
        let damaged = pristine.crash(Crash::DropUnsynced);
        let file = damaged.open(NAME, block_size().unwrap()).unwrap();
        let block = offset / BLOCK as u64;
        let mut bytes = vec![0; BLOCK];
        file.read(block, &mut bytes).unwrap();
        bytes[(offset % BLOCK as u64) as usize] ^= 0x20;
        file.write(block, &bytes).unwrap();
        let outcome = Engine::open(Arc::new(damaged), NAME, EngineOptions::default())
            .map_err(StorageError::from)
            .and_then(|engine| {
                let found = everything(&engine);
                if found.is_ok() {
                    engine.check().map_err(StorageError::from)?;
                }
                found
            });
        match outcome {
            Ok(found) => {
                assert!(
                    found == expected,
                    "damage at byte {offset} changed what reads return"
                );
                outcomes[0] += 1;
            }
            Err(error) => {
                assert!(
                    error.is_corrupt(),
                    "damage at byte {offset} failed with {error}"
                );
                outcomes[1] += 1;
            }
        }
        offset += stride + offset % 7;
    }
    assert!(outcomes[0] > 0 && outcomes[1] > 0, "{outcomes:?}");
}

#[test]
fn readers_keep_their_snapshots_while_background_checkpoints_publish_new_trees() {
    let fs = SimFs::new();
    let engine =
        Engine::open(Arc::new(fs.clone()), NAME, tiny(CheckpointMode::Background)).unwrap();
    fill(&engine, 2);
    let pinned = engine.begin_read().unwrap();
    let before = pinned.scan_prefix(Table::Nodes, &[]).unwrap();
    std::thread::scope(|scope| {
        scope.spawn(|| fill(&engine, 60));
        for _ in 0..200 {
            assert_eq!(pinned.scan_prefix(Table::Nodes, &[]).unwrap(), before);
            let fresh = engine.begin_read().unwrap();
            let keys = fresh.scan_prefix(Table::Nodes, &[]).unwrap();
            assert!(keys.windows(2).all(|pair| pair[0].0 < pair[1].0));
        }
    });
    drop(pinned);
    engine.checkpoint().unwrap();
    assert!(engine.header().generation > 4);
    engine.check().unwrap();
}

#[test]
fn deleted_keys_stay_deleted_whether_or_not_the_file_holds_them() {
    let engine =
        Engine::open(Arc::new(SimFs::new()), NAME, manual(CheckpointMode::Inline)).unwrap();
    let commit = |puts: &[&[u8]], deletes: &[&[u8]]| {
        let mut write = engine.begin_write().unwrap();
        for key in puts {
            write.put(Table::Nodes, key, b"v").unwrap();
        }
        for key in deletes {
            write.delete(Table::Nodes, key).unwrap();
        }
        write.commit().unwrap();
    };
    commit(&[b"filed"], &[]);
    engine.checkpoint().unwrap();
    commit(&[b"fresh"], &[]);
    let before = engine.begin_read().unwrap();
    commit(&[], &[b"filed", b"fresh"]);
    for round in 0..4u8 {
        commit(&[&[round]], &[]);
    }

    assert_eq!(
        before.get(Table::Nodes, b"filed").unwrap().as_deref(),
        Some(&b"v"[..])
    );
    assert_eq!(
        before.get(Table::Nodes, b"fresh").unwrap().as_deref(),
        Some(&b"v"[..])
    );
    drop(before);
    for round in 4..8u8 {
        commit(&[&[round]], &[]);
    }
    let now = engine.begin_read().unwrap();
    assert_eq!(now.get(Table::Nodes, b"filed").unwrap(), None);
    assert_eq!(now.get(Table::Nodes, b"fresh").unwrap(), None);
    let keys: Vec<Vec<u8>> = now
        .scan_prefix(Table::Nodes, &[])
        .unwrap()
        .into_iter()
        .map(|(key, _)| key)
        .collect();
    assert_eq!(keys, (0..8u8).map(|round| vec![round]).collect::<Vec<_>>());
    drop(now);
    engine.checkpoint().unwrap();
    let reread = engine.begin_read().unwrap();
    assert_eq!(reread.get(Table::Nodes, b"filed").unwrap(), None);
    assert_eq!(reread.scan_prefix(Table::Nodes, &[]).unwrap().len(), 8);
}

#[derive(Clone, Debug)]
struct RemovalThreads {
    fs: SimFs,
    removers: Arc<std::sync::Mutex<Vec<Option<String>>>>,
}

impl FileSystem for RemovalThreads {
    type File = <SimFs as FileSystem>::File;

    fn create(&self, name: &str, size: BlockSize) -> io::Result<Self::File> {
        self.fs.create(name, size)
    }

    fn open(&self, name: &str, size: BlockSize) -> io::Result<Self::File> {
        self.fs.open(name, size)
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        self.fs.rename(from, to)
    }

    fn remove(&self, name: &str) -> io::Result<()> {
        if name.contains("-wal-") {
            let thread = std::thread::current().name().map(str::to_owned);
            self.removers.lock().unwrap().push(thread);
        }
        self.fs.remove(name)
    }

    fn names(&self) -> io::Result<Vec<String>> {
        self.fs.names()
    }

    fn sync_names(&self) -> io::Result<()> {
        self.fs.sync_names()
    }
}

#[test]
fn commits_leave_removing_the_log_a_checkpoint_freed_to_the_checkpoint_thread() {
    let fs = RemovalThreads {
        fs: SimFs::new(),
        removers: Arc::default(),
    };
    let engine =
        Engine::open(Arc::new(fs.clone()), NAME, tiny(CheckpointMode::Background)).unwrap();

    fill(&engine, 60);
    engine.shared.wait_for_checkpoint();
    fill(&engine, 1);
    engine.shared.wait_for_checkpoint();
    let before_close = fs.removers.lock().unwrap().clone();
    let committed = everything(&engine).unwrap();
    engine.close().unwrap();

    let current = std::thread::current().name().map(str::to_owned);
    assert!(!before_close.is_empty());
    assert!(
        before_close.iter().all(|thread| *thread != current),
        "{before_close:?}"
    );
    let reopened = Engine::open(
        Arc::new(fs.fs.clone()),
        NAME,
        tiny(CheckpointMode::Background),
    )
    .unwrap();
    assert_eq!(everything(&reopened).unwrap(), committed);
}

#[test]
fn the_checkpoint_thread_frees_a_frozen_memtable_that_a_commit_held_last() {
    let engine = Engine::open(
        Arc::new(SimFs::new()),
        NAME,
        manual(CheckpointMode::Background),
    )
    .unwrap();
    fill(&engine, 4);
    let checkpoint_held = lock(&engine.shared.durable);
    engine.start_checkpoint(&mut lock(&engine.shared.writer));
    let frozen = {
        let published = lock(&engine.shared.published);
        Arc::downgrade(published.view.layers.frozen.as_ref().unwrap())
    };
    let mut write = engine.begin_write().unwrap();
    write.put(Table::Nodes, b"late", b"v").unwrap();
    drop(checkpoint_held);
    engine.shared.wait_for_checkpoint();
    engine.shared.drop_segments(&mut write.writer);

    write.commit().unwrap();

    assert!(
        frozen.upgrade().is_some(),
        "the commit freed the frozen memtable"
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while frozen.strong_count() > 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(frozen.strong_count(), 0);
    let read = engine.begin_read().unwrap();
    assert_eq!(
        read.get(Table::Nodes, b"late").unwrap().as_deref(),
        Some(&b"v"[..])
    );
}

#[test]
fn the_engine_opens_on_a_directory_and_in_memory() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph.rhizo");
    let engine = crate::engine::any_fs::open_path(&path, EngineOptions::default()).unwrap();
    fill(&engine, 3);
    let expected = everything(&engine).unwrap();
    drop(engine);
    let names: Vec<String> = std::fs::read_dir(directory.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(names, ["graph.rhizo"]);
    let engine = crate::engine::any_fs::open_path(&path, EngineOptions::default()).unwrap();
    assert_eq!(everything(&engine).unwrap(), expected);
    engine.check().unwrap();
    let memory = crate::engine::any_fs::in_memory(EngineOptions::default()).unwrap();
    fill(&memory, 3);
    assert_eq!(everything(&memory).unwrap(), expected);
}

fn put_round(write: &mut impl WriteTransaction, round: u64) {
    for item in 0..100u64 {
        write
            .put(
                Table::Nodes,
                &(round * 100 + item).to_be_bytes(),
                &[7u8; 40],
            )
            .unwrap();
    }
}

fn probe_earlier_commits_while_checkpointing(checkpoint: CheckpointMode, rounds: u64) -> u64 {
    let options = EngineOptions {
        memtable_limit: 64 << 10,
        ..tiny(checkpoint)
    };
    let engine = Engine::open(Arc::new(MemFs::default()), NAME, options).unwrap();
    let mut missing = 0;
    for round in 0..rounds {
        let mut write = engine.begin_write().unwrap();
        for step in 0..8u64 {
            if round == 0 {
                break;
            }
            let probe =
                (round.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 7).wrapping_add(step * 977) % round;
            if write
                .get(Table::Nodes, &(probe * 100).to_be_bytes())
                .unwrap()
                .is_none()
            {
                missing += 1;
            }
        }
        put_round(&mut write, round);
        write.commit().unwrap();
    }
    missing
}

fn range_deletes_across_checkpoints_then_reopen(checkpoint: CheckpointMode, rounds: u64) -> usize {
    let fs = Arc::new(MemFs::default());
    let options = EngineOptions {
        memtable_limit: 64 << 10,
        ..tiny(checkpoint)
    };
    {
        let engine = Engine::open(Arc::clone(&fs), NAME, options).unwrap();
        for round in 0..rounds {
            let mut write = engine.begin_write().unwrap();
            put_round(&mut write, round);
            if round >= 3 {
                let old = round - 3;
                write
                    .delete_range(
                        Table::Nodes,
                        &(old * 100).to_be_bytes(),
                        &((old + 1) * 100).to_be_bytes(),
                    )
                    .unwrap();
            }
            write.commit().unwrap();
        }
    }
    let engine = Engine::open(fs, NAME, options).unwrap();
    engine.check().unwrap();
    let read = engine.begin_read().unwrap();
    read.scan_prefix(Table::Nodes, &[]).unwrap().len()
}

fn race_rounds() -> u64 {
    u64::from(test_scale::cases(6000)).min(60_000)
}

#[test]
fn a_writer_sees_every_earlier_commit_while_checkpoints_publish() {
    for checkpoint in [CheckpointMode::Background, CheckpointMode::Inline] {
        assert_eq!(
            probe_earlier_commits_while_checkpointing(checkpoint, race_rounds()),
            0,
            "{checkpoint:?}"
        );
    }
}

#[test]
fn range_deletes_stay_deleted_across_checkpoints_and_a_reopen() {
    for checkpoint in [CheckpointMode::Background, CheckpointMode::Inline] {
        assert_eq!(
            range_deletes_across_checkpoints_then_reopen(checkpoint, race_rounds()),
            300,
            "{checkpoint:?}"
        );
    }
}

#[test]
fn a_checkpoint_that_meets_a_damaged_page_reports_corruption_from_then_on() {
    let fs = SimFs::new();
    let options = EngineOptions {
        memtable_limit: u64::MAX,
        wal_limit: u64::MAX,
        ..tiny(CheckpointMode::Inline)
    };
    let engine = Engine::open(Arc::new(fs.clone()), NAME, options).unwrap();
    fill(&engine, 3);
    engine.checkpoint().unwrap();
    let extents = engine.extents();
    let catalog = engine.header().catalog;
    drop(engine);
    let extent = extents.entries[&catalog.extent()];
    let file = fs.open(NAME, block_size().unwrap()).unwrap();
    for block in 0..catalog.block() {
        let at = physical_block(extent.slot, block).unwrap();
        let mut bytes = vec![0; BLOCK];
        file.read(at, &mut bytes).unwrap();
        bytes[100] ^= 1;
        file.write(at, &bytes).unwrap();
    }
    let engine = Engine::open(Arc::new(fs.clone()), NAME, options).unwrap();
    fill(&engine, 1);
    let error = engine.checkpoint().unwrap_err();
    assert!(error.is_corrupt(), "{error}");
    let refused = engine.begin_write().map(drop).unwrap_err();
    assert!(refused.is_corrupt(), "{refused}");
}

fn file_bytes(fs: &SimFs) -> u64 {
    fs.open(NAME, block_size().unwrap())
        .unwrap()
        .block_count()
        .unwrap()
        * BLOCK as u64
}

fn churn(engine: &impl Storage, rounds: u32) {
    for round in 0..rounds {
        let mut write = engine.begin_write().unwrap();
        for item in 0..300u32 {
            let key = (item * 13 + round * 7) % 2000;
            let length = [20, 300, 2000][(item % 3) as usize];
            write
                .put(Table::Nodes, &key.to_be_bytes(), &vec![round as u8; length])
                .unwrap();
        }
        write
            .delete_range(
                Table::Nodes,
                &(round % 2000).to_be_bytes(),
                &(round % 2000 + 20).to_be_bytes(),
            )
            .unwrap();
        write.commit().unwrap();
    }
}

#[test]
fn compacting_shrinks_the_file_to_its_live_data_and_keeps_every_entry() {
    let fs = SimFs::new();
    let engine = Engine::open(Arc::new(fs.clone()), NAME, tiny(CheckpointMode::Inline)).unwrap();
    churn(&engine, 60);
    let expected = everything(&engine).unwrap();
    let before = file_bytes(&fs);
    engine.compact().unwrap();
    let report = engine.check().unwrap();
    let after = file_bytes(&fs);
    assert_eq!(everything(&engine).unwrap(), expected);
    let live = report.live_blocks * BLOCK as u64;
    let extents = engine.extents();
    assert_eq!(
        extents.entries.len() as u64,
        extents.next_slot,
        "compacting left free slots"
    );
    let extent_bytes = u64::from(crate::engine::layout::EXTENT_BLOCKS) * BLOCK as u64;
    assert!(extents.entries.len() as u64 <= 3 + live / extent_bytes);
    assert!(after < before, "compacting left {after} of {before} bytes");
    churn(&engine, 5);
    let expected = everything(&engine).unwrap();
    let crashed = fs.crash(Crash::DropUnsynced);
    drop(engine);
    let engine = Engine::open(Arc::new(crashed), NAME, EngineOptions::default()).unwrap();
    assert_eq!(everything(&engine).unwrap(), expected);
    engine.check().unwrap();
}

#[test]
fn a_reader_holding_an_old_snapshot_through_a_compaction_reads_it_whole() {
    let fs = SimFs::new();
    let engine = Engine::open(Arc::new(fs.clone()), NAME, tiny(CheckpointMode::Inline)).unwrap();
    churn(&engine, 30);
    let pinned = engine.begin_read().unwrap();
    let before = pinned.scan_prefix(Table::Nodes, &[]).unwrap();
    engine.compact().unwrap();
    churn(&engine, 30);
    engine.compact().unwrap();
    assert_eq!(pinned.scan_prefix(Table::Nodes, &[]).unwrap(), before);
    drop(pinned);
    let grown = file_bytes(&fs);
    engine.compact().unwrap();
    assert!(file_bytes(&fs) < grown);
    engine.check().unwrap();
}

#[test]
fn overwriting_the_same_keys_keeps_the_file_bounded() {
    let fs = SimFs::new();
    let engine = Engine::open(Arc::new(fs.clone()), NAME, tiny(CheckpointMode::Inline)).unwrap();
    churn(&engine, 40);
    let early = file_bytes(&fs);
    churn(&engine, 160);
    let late = file_bytes(&fs);
    assert!(
        late < early * 3,
        "the file grew from {early} to {late} bytes rewriting the same keys"
    );
    engine.check().unwrap();
}

fn load_then_keep_survivors(engine: &Engine<SimFs>, rounds: u32) {
    let key = |round: u32, item: u32| (round * 200 + item).to_be_bytes();
    for round in 0..rounds {
        let mut write = engine.begin_write().unwrap();
        for item in 0..200 {
            write
                .put(Table::Nodes, &key(round, item), &[round as u8; 300])
                .unwrap();
        }
        write.commit().unwrap();
    }
    let mut write = engine.begin_write().unwrap();
    for round in 0..rounds {
        for item in (0..200).filter(|item| item % 50 != 0) {
            write.delete(Table::Nodes, &key(round, item)).unwrap();
        }
    }
    write.commit().unwrap();
    engine.checkpoint().unwrap();
}

fn file_and_live_bytes_after_most_is_deleted(reclaim_slack: u64) -> (u64, u64) {
    let fs = SimFs::new();
    let options = EngineOptions {
        reclaim_slack,
        memtable_limit: 256 << 10,
        wal_limit: 1 << 20,
        ..tiny(CheckpointMode::Inline)
    };
    let engine = Engine::open(Arc::new(fs.clone()), NAME, options).unwrap();
    load_then_keep_survivors(&engine, 400);
    let live = engine.check().unwrap().live_blocks * BLOCK as u64;
    assert_eq!(
        everything(&engine).unwrap().len(),
        400 * 4,
        "every survivor"
    );
    (file_bytes(&fs), live)
}

#[test]
fn checkpoints_compact_a_file_whose_free_space_outgrows_its_live_data() {
    let slack = tiny(CheckpointMode::Inline).reclaim_slack;
    let extent = u64::from(crate::engine::layout::EXTENT_BLOCKS) * BLOCK as u64;
    let bound = |live: u64| 2 * live + slack + 4 * extent;
    let (kept, kept_live) = file_and_live_bytes_after_most_is_deleted(u64::MAX);
    assert!(
        kept > bound(kept_live),
        "without reclaiming, the deletes leave {kept} bytes for {kept_live} live"
    );
    let (reclaimed, live) = file_and_live_bytes_after_most_is_deleted(slack);
    assert!(
        reclaimed <= bound(live),
        "the file holds {reclaimed} bytes for {live} live bytes"
    );
}

#[test]
fn a_database_file_open_on_disk_refuses_a_second_open_until_it_closes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph");
    let first = crate::engine::any_fs::open_path(&path, EngineOptions::default()).unwrap();
    let error = crate::engine::any_fs::open_path(&path, EngineOptions::default()).unwrap_err();
    assert!(
        matches!(&error, EngineError::Io(io) if io.kind() == io::ErrorKind::WouldBlock),
        "{error:?}"
    );
    drop(first);
    crate::engine::any_fs::open_path(&path, EngineOptions::default()).unwrap();
}

#[test]
fn an_empty_file_without_a_log_opens_as_a_new_database_and_a_short_one_does_not() {
    let directory = tempfile::tempdir().unwrap();
    let empty = directory.path().join("empty");
    std::fs::write(&empty, b"").unwrap();
    let engine = crate::engine::any_fs::open_path(&empty, EngineOptions::default()).unwrap();
    let mut tx = engine.begin_write().unwrap();
    tx.put(Table::Metadata, b"k", b"v").unwrap();
    tx.commit().unwrap();
    drop(engine);
    let engine = crate::engine::any_fs::open_path(&empty, EngineOptions::default()).unwrap();
    assert_eq!(
        engine
            .begin_read()
            .unwrap()
            .get(Table::Metadata, b"k")
            .unwrap(),
        Some(b"v".to_vec())
    );
    let short = directory.path().join("short");
    std::fs::write(&short, [7_u8; 100]).unwrap();
    assert!(crate::engine::any_fs::open_path(&short, EngineOptions::default()).is_err());
    assert_eq!(std::fs::read(&short).unwrap(), [7_u8; 100]);
}

fn names_in(directory: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

#[test]
fn a_staging_file_left_by_an_interrupted_open_is_reused_or_removed_and_never_replaces_a_database() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph");
    std::fs::write(directory.path().join(".graph.creating"), [9_u8; 5_000]).unwrap();
    let engine = crate::engine::any_fs::open_path(&path, EngineOptions::default()).unwrap();
    let mut tx = engine.begin_write().unwrap();
    tx.put(Table::Metadata, b"k", b"v").unwrap();
    tx.commit().unwrap();
    drop(engine);
    assert_eq!(names_in(directory.path()), ["graph"]);
    std::fs::write(directory.path().join(".graph.creating"), [9_u8; 5_000]).unwrap();
    let engine = crate::engine::any_fs::open_path(&path, EngineOptions::default()).unwrap();
    assert_eq!(
        engine
            .begin_read()
            .unwrap()
            .get(Table::Metadata, b"k")
            .unwrap(),
        Some(b"v".to_vec())
    );
    drop(engine);
    assert_eq!(names_in(directory.path()), ["graph"]);
}

#[test]
fn opens_racing_to_create_a_database_share_one_file_and_lose_no_commit() {
    for _ in 0..50 {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("graph");
        let opened: Vec<_> = std::thread::scope(|scope| {
            let openers: Vec<_> = (0..8_u8)
                .map(|opener| {
                    let path = &path;
                    scope.spawn(move || {
                        let engine =
                            crate::engine::any_fs::open_path(path, EngineOptions::default())
                                .ok()?;
                        let mut tx = engine.begin_write().unwrap();
                        tx.put(Table::Metadata, &[opener], b"v").unwrap();
                        tx.commit().unwrap();
                        Some(opener)
                    })
                })
                .collect();
            openers
                .into_iter()
                .filter_map(|opener| opener.join().unwrap())
                .collect()
        });
        assert!(!opened.is_empty());
        let engine = crate::engine::any_fs::open_path(&path, EngineOptions::default()).unwrap();
        let read = engine.begin_read().unwrap();
        for opener in opened {
            assert_eq!(
                read.get(Table::Metadata, &[opener]).unwrap(),
                Some(b"v".to_vec())
            );
        }
        drop(read);
        drop(engine);
        assert_eq!(names_in(directory.path()), ["graph"]);
    }
}

fn load_items(engine: &Engine<SimFs>, round: u32) {
    for batch in 0..12u32 {
        let mut write = engine.begin_write().unwrap();
        for item in 0..250u32 {
            let key = batch * 250 + item;
            let length = [40, 400, 1500][(key % 3) as usize];
            write
                .put(Table::Nodes, &key.to_be_bytes(), &vec![round as u8; length])
                .unwrap();
        }
        write.commit().unwrap();
        engine.checkpoint().unwrap();
    }
}

fn delete_items(engine: &Engine<SimFs>) {
    let mut write = engine.begin_write().unwrap();
    write.delete_range(Table::Nodes, &[], &[0xFF; 5]).unwrap();
    write.commit().unwrap();
    engine.checkpoint().unwrap();
}

fn without_compaction() -> EngineOptions {
    EngineOptions {
        reclaim_slack: u64::MAX,
        ..manual(CheckpointMode::Inline)
    }
}

#[test]
fn loading_deleting_and_loading_again_keeps_the_file_near_one_load() {
    let fs = SimFs::new();
    let engine = Engine::open(Arc::new(fs.clone()), NAME, without_compaction()).unwrap();
    let mut write = engine.begin_write().unwrap();
    write.put(Table::Metadata, b"version", b"1").unwrap();
    write.commit().unwrap();
    load_items(&engine, 0);
    let one_load = file_bytes(&fs);
    let mut largest = 0;
    for round in 1..8 {
        delete_items(&engine);
        engine.check().unwrap();
        load_items(&engine, round);
        engine.check().unwrap();
        largest = largest.max(file_bytes(&fs));
    }
    let extent = u64::from(crate::engine::layout::EXTENT_BLOCKS) * BLOCK as u64;
    assert!(
        largest <= one_load + extent,
        "one load takes {one_load} bytes, but loading again grew the file to {largest}"
    );
    assert_eq!(everything(&engine).unwrap().len(), 3001);
}

#[test]
fn rewriting_a_third_of_the_keys_at_every_checkpoint_keeps_the_file_near_its_live_data() {
    let fs = SimFs::new();
    let engine = Engine::open(Arc::new(fs.clone()), NAME, without_compaction()).unwrap();
    load_items(&engine, 0);
    let mut sizes = Vec::new();
    for round in 1..40u32 {
        let mut write = engine.begin_write().unwrap();
        for key in (0..3000u32).filter(|key| key % 3 == round % 3) {
            let length = [40, 400, 1500][((key + round) % 3) as usize];
            write
                .put(Table::Nodes, &key.to_be_bytes(), &vec![round as u8; length])
                .unwrap();
        }
        write.commit().unwrap();
        engine.checkpoint().unwrap();
        sizes.push(file_bytes(&fs));
    }
    let live = engine.check().unwrap().live_blocks * BLOCK as u64;
    let extent = u64::from(crate::engine::layout::EXTENT_BLOCKS) * BLOCK as u64;
    let largest = sizes.iter().max().copied().unwrap_or(0);
    assert!(
        largest <= 2 * live + extent,
        "the file reached {largest} bytes for {live} live bytes: {sizes:?}"
    );
}
