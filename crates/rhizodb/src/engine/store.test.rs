use super::*;
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
        all.extend(read.scan_prefix(table, &[])?);
    }
    for (key, value) in all.clone() {
        if read.get(Table::Nodes, &key)?.as_ref() != Some(&value) {
            return Err(StorageError::new(EngineError::corrupt(
                "a get differs from the scan",
            )));
        }
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

fn written_blocks(fs: &SimFs, extents: &ExtentTable) -> Vec<(u64, Vec<u8>)> {
    let file = fs.open(NAME, block_size().unwrap()).unwrap();
    let mut blocks = Vec::new();
    for entry in extents.entries.values() {
        for block in 0..entry.written {
            let at = physical_block(entry.slot, block).unwrap();
            let mut bytes = vec![0; BLOCK];
            file.read(at, &mut bytes).unwrap();
            blocks.push((at, bytes));
        }
    }
    blocks
}

#[test]
fn no_block_a_durable_header_reached_is_ever_written_again() {
    let fs = SimFs::new();
    let engine = Engine::open(Arc::new(fs.clone()), NAME, tiny(CheckpointMode::Inline)).unwrap();
    let mut snapshots = Vec::new();
    for _ in 0..12 {
        fill(&engine, 3);
        let mut write = engine.begin_write().unwrap();
        write
            .delete_range(Table::Nodes, &[0, 0, 0, 10], &[0, 0, 0, 40])
            .unwrap();
        write.commit().unwrap();
        engine.check().unwrap();
        snapshots.push(written_blocks(&fs, &engine.extents()));
    }
    drop(engine);
    let file = fs.open(NAME, block_size().unwrap()).unwrap();
    let mut checked = 0;
    for snapshot in snapshots {
        for (at, bytes) in snapshot {
            let mut now = vec![0; BLOCK];
            file.read(at, &mut now).unwrap();
            assert!(now == bytes, "block {at} changed after a header reached it");
            checked += 1;
        }
    }
    assert!(checked > 1000);
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
