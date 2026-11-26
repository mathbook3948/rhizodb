use std::collections::BTreeMap;
use std::io;
use std::io::Read;
use std::sync::Arc;

use super::CHUNK;
use super::Export;
use super::install;
use crate::engine::block_file::BlockFile;
use crate::engine::block_file::FileSystem;
use crate::engine::error::EngineError;
use crate::engine::layout::WAL_BLOCK;
use crate::engine::sim_fs::Crash;
use crate::engine::sim_fs::Failure;
use crate::engine::sim_fs::SimFs;
use crate::engine::store::CheckpointMode;
use crate::engine::store::Engine;
use crate::engine::store::EngineOptions;
use crate::engine::wal::segment_files;
use crate::engine::wal::segment_name;
use crate::engine::wal_format::SegmentHeader;
use crate::engine::wal_format::decode_record;
use crate::engine::wal_format::padded;
use crate::error::ErrorKind;
use crate::storage::Maintenance;
use crate::storage::ReadTransaction;
use crate::storage::Storage;
use crate::storage::StorageError;
use crate::storage::Table;
use crate::storage::WriteTransaction;
use crate::test_scale;

const SOURCE: &str = "source.rhizo";
const TARGET: &str = "replica.rhizo";
const TABLES: [Table; 3] = [Table::Metadata, Table::NodeColumns, Table::PropertyEntries];

type State = BTreeMap<(Table, Vec<u8>), Vec<u8>>;

fn options() -> EngineOptions {
    EngineOptions {
        segment_target: 8192,
        memtable_limit: 6_000,
        wal_limit: 20_000,
        checkpoint: CheckpointMode::Inline,
        cache_bytes: 64 * crate::engine::layout::BLOCK,
        reclaim_slack: 64 << 10,
        stage_floor: 8,
        max_retained_bytes: u64::MAX,
        pin_limit: EngineOptions::default().pin_limit,
    }
}

fn by_hand() -> EngineOptions {
    EngineOptions {
        memtable_limit: u64::MAX,
        wal_limit: u64::MAX,
        reclaim_slack: u64::MAX,
        ..options()
    }
}

fn open(fs: &SimFs, name: &str, options: EngineOptions) -> Result<Engine<SimFs>, EngineError> {
    Engine::open(Arc::new(fs.clone()), name, options)
}

fn dump(engine: &impl Storage) -> State {
    let read = engine.begin_read().unwrap();
    let mut state = State::new();
    for table in Table::ALL {
        for (key, value) in read.scan_prefix(table, &[]).unwrap() {
            state.insert((table, key), value);
        }
    }
    state
}

fn digest(state: &State) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(state, &mut hasher);
    std::hash::Hasher::finish(&hasher)
}

fn value(round: u64, index: u64) -> Vec<u8> {
    let length = match (round + index) % 9 {
        0 => 70_000,
        1 => 3_000,
        _ => ((round * 31 + index * 7) % 200) as usize,
    };
    (0..length)
        .map(|at| (at as u64 + round + index) as u8)
        .collect()
}

fn write_round(engine: &impl Storage, round: u64, rows: u64) {
    let mut write = engine.begin_write().unwrap();
    for index in 0..rows {
        let table = TABLES[(index % 3) as usize];
        let key = (index * 7 + round).to_be_bytes();
        write.put(table, &key, &value(round, index)).unwrap();
    }
    if round % 3 == 2 {
        let start = (round * 5).to_be_bytes();
        let end = (round * 5 + rows * 2).to_be_bytes();
        write
            .delete_range(Table::NodeColumns, &start, &end)
            .unwrap();
        write
            .delete(Table::Metadata, &(round * 3).to_be_bytes())
            .unwrap();
    }
    write.commit().unwrap();
}

fn read_all<F: FileSystem + 'static>(export: &Export<'_, F>, step: usize) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut buffer = vec![0; step];
    loop {
        let read = export.read_at(bytes.len() as u64, &mut buffer).unwrap();
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..read]);
    }
    assert_eq!(bytes.len() as u64, export.len());
    bytes
}

fn loaded_source(rounds: u64, rows: u64) -> (SimFs, Engine<SimFs>, State) {
    let fs = SimFs::new();
    let engine = open(&fs, SOURCE, by_hand()).unwrap();
    for round in 0..rounds {
        write_round(&engine, round, rows);
    }
    engine.checkpoint().unwrap();
    let state = dump(&engine);
    (fs, engine, state)
}

fn install_bytes(fs: &SimFs, bytes: &[u8]) -> Result<Engine<SimFs>, EngineError> {
    install(Arc::new(fs.clone()), TARGET, bytes, options(), true)
}

fn old_target() -> (SimFs, State) {
    let fs = SimFs::new();
    let engine = open(&fs, TARGET, by_hand()).unwrap();
    for round in 100..104 {
        write_round(&engine, round, 12);
    }
    engine.checkpoint().unwrap();
    write_round(&engine, 104, 12);
    let state = dump(&engine);
    let crashed = fs.crash(Crash::KeepUnsynced);
    drop(engine);
    (crashed, state)
}

#[test]
fn an_installed_snapshot_holds_the_checkpoint_it_pinned() {
    let (_fs, source, expected) = loaded_source(12, 60);
    let position = source.header().checkpoint_lsn;
    let export = source.export(b"history").unwrap();
    write_round(&source, 50, 60);
    source.checkpoint().unwrap();
    assert_eq!(export.position(), position);
    assert!(export.len() > 2 * CHUNK as u64, "the export is one chunk");
    let bytes = read_all(&export, 7_777);
    assert_eq!(read_all(&export, CHUNK + 8), bytes);
    let target = SimFs::new();
    let installed = install_bytes(&target, &bytes).unwrap();
    assert_eq!(installed.header().checkpoint_lsn, position);
    installed.check().unwrap();
    assert_eq!(dump(&installed), expected);
    let refused = installed.begin_write().and_then(|mut write| {
        write.put(Table::Metadata, b"local", b"write")?;
        write.commit()
    });
    assert_eq!(refused.unwrap_err().kind(), ErrorKind::NotWriter);
    drop(installed);
    let reopened = open(&target, TARGET, options()).unwrap();
    assert_eq!(dump(&reopened), expected);
}

#[test]
fn an_empty_database_exports_and_installs() {
    let fs = SimFs::new();
    let source = open(&fs, SOURCE, by_hand()).unwrap();
    let export = source.export(&[]).unwrap();
    let bytes = read_all(&export, 100);
    let installed = install_bytes(&SimFs::new(), &bytes).unwrap();
    assert_eq!(installed.header().checkpoint_lsn, 0);
    assert!(dump(&installed).is_empty());
}

#[test]
fn installing_replaces_an_existing_database_and_its_log() {
    let (_fs, source, expected) = loaded_source(5, 30);
    let bytes = read_all(&source.export(&[]).unwrap(), 1 << 16);
    let (target, old) = old_target();
    assert_ne!(old, expected);
    let installed = install_bytes(&target, &bytes).unwrap();
    assert_eq!(dump(&installed), expected);
    let crashed = target.crash(Crash::DropUnsynced);
    drop(installed);
    assert_eq!(dump(&open(&crashed, TARGET, options()).unwrap()), expected);
    assert_eq!(dump(&open(&target, TARGET, options()).unwrap()), expected);
}

#[test]
fn installing_over_an_open_database_fails_and_keeps_it() {
    let (_fs, source, _) = loaded_source(3, 10);
    let bytes = read_all(&source.export(&[]).unwrap(), 1 << 16);
    let target = tempfile::tempdir().unwrap();
    let path = target.path().join(TARGET);
    let open = crate::engine::any_fs::open_path(&path, by_hand()).unwrap();
    write_round(&open, 1, 5);
    let fs = Arc::new(crate::engine::any_fs::AnyFs::Os(
        crate::engine::os_fs::OsFs::new(target.path().to_path_buf()),
    ));
    let error = install(fs, TARGET, bytes.as_slice(), options(), false).unwrap_err();
    assert_eq!(
        StorageError::from(error).kind(),
        ErrorKind::DatabaseAlreadyOpen
    );
    write_round(&open, 2, 5);
}

fn flipped(bytes: &[u8], at: usize) -> Vec<u8> {
    let mut damaged = bytes.to_vec();
    damaged[at] ^= 1 << (at % 8);
    damaged
}

#[test]
#[ignore = "left out of mise run check, run by mise run random"]
fn a_damaged_or_cut_snapshot_fails_as_corrupt_data_and_keeps_the_old_database() {
    let (_fs, source, _) = loaded_source(8, 40);
    let bytes = read_all(&source.export(&[]).unwrap(), 1 << 16);
    let (target, old) = old_target();
    let frame = CHUNK + 8;
    let mut offsets: Vec<usize> = (0..200).collect();
    for start in (0..bytes.len()).step_by(frame) {
        offsets.extend([start, start + 1, start + frame - 8, start + frame - 1]);
    }
    let mut random = 17u64;
    for _ in 0..test_scale::cases(40) {
        random = random
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        offsets.push((random >> 33) as usize % bytes.len());
    }
    offsets.retain(|at| *at < bytes.len());
    offsets.push(bytes.len() - 1);
    for at in offsets {
        let error = install_bytes(&target, &flipped(&bytes, at))
            .err()
            .unwrap_or_else(|| panic!("a flip at byte {at} installed"));
        assert!(error.is_corrupt(), "a flip at byte {at} failed as {error}");
    }
    for length in [0, 7, 24, 100, CHUNK, frame, frame + 1, bytes.len() - 1] {
        let error = install_bytes(&target, &bytes[..length])
            .err()
            .unwrap_or_else(|| panic!("{length} bytes installed"));
        assert!(error.is_corrupt(), "{length} bytes failed as {error}");
    }
    let mut other = read_all(&source.export(&[]).unwrap(), 1 << 16);
    other[frame..2 * frame].copy_from_slice(&bytes[frame..2 * frame]);
    assert!(install_bytes(&target, &other).unwrap_err().is_corrupt());
    assert_eq!(dump(&open(&target, TARGET, options()).unwrap()), old);
}

struct Connection<'a, 'b> {
    export: &'a Export<'b, SimFs>,
    at: u64,
    cuts: Vec<u64>,
    resume: bool,
}

impl Read for Connection<'_, '_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let limit = self.cuts.first().copied().unwrap_or(u64::MAX);
        if self.at >= limit {
            self.cuts.remove(0);
            if !self.resume {
                return Err(io::Error::new(io::ErrorKind::ConnectionReset, "cut"));
            }
        }
        let room = (limit.saturating_sub(self.at) as usize).clamp(1, buffer.len().min(50_000));
        let read = self
            .export
            .read_at(self.at, &mut buffer[..room])
            .map_err(io::Error::other)?;
        self.at += read as u64;
        Ok(read)
    }
}

#[test]
fn a_cut_transfer_fails_cleanly_and_a_resumed_one_installs() {
    let (_fs, source, expected) = loaded_source(10, 50);
    let export = source.export(&[]).unwrap();
    let (target, old) = old_target();
    let cut = Connection {
        export: &export,
        at: 0,
        cuts: vec![export.len() / 2],
        resume: false,
    };
    let error = install(Arc::new(target.clone()), TARGET, cut, options(), true).unwrap_err();
    assert_eq!(StorageError::from(error).kind(), ErrorKind::Io);
    assert_eq!(dump(&open(&target, TARGET, options()).unwrap()), old);
    let cuts = (1..6).map(|part| export.len() * part / 6 + part).collect();
    let resumed = Connection {
        export: &export,
        at: 0,
        cuts,
        resume: true,
    };
    let installed = install(Arc::new(target.clone()), TARGET, resumed, options(), true).unwrap();
    assert_eq!(dump(&installed), expected);
}

fn crashes() -> Vec<Crash> {
    let mut crashes = vec![Crash::DropUnsynced, Crash::KeepUnsynced];
    for seed in 0..2 {
        crashes.push(Crash::Scramble { seed });
        crashes.push(Crash::Reorder { seed });
    }
    crashes
}

#[test]
#[ignore = "left out of mise run check, run by mise run random"]
fn every_failure_then_power_loss_while_installing_leaves_the_old_database_or_the_new() {
    let (_fs, source, expected) = loaded_source(4, 20);
    let bytes = read_all(&source.export(&[]).unwrap(), 1 << 16);
    let (base, old) = old_target();
    let total = {
        let copy = base.crash(Crash::DropUnsynced);
        let before = copy.operations();
        drop(install_bytes(&copy, &bytes).unwrap());
        copy.operations() - before
    };
    assert!(total > 10, "installing ran only {total} operations");
    for fail_at in 0..=total {
        let copy = base.crash(Crash::DropUnsynced);
        copy.fail_after(fail_at, Failure::FromThenOn);
        let installed = install_bytes(&copy, &bytes).is_ok();
        for crash in crashes() {
            let reopened = open(&copy.crash(crash), TARGET, options())
                .unwrap_or_else(|error| panic!("fail at {fail_at}, {crash:?}: {error}"));
            reopened.check().unwrap_or_else(|error| {
                panic!("check after fail at {fail_at}, {crash:?}: {error}")
            });
            let found = dump(&reopened);
            assert!(
                found == old || found == expected,
                "fail at {fail_at}, {crash:?}: neither the old database nor the snapshot"
            );
            if installed && crash == Crash::DropUnsynced {
                assert_eq!(
                    found, expected,
                    "fail at {fail_at}: an install that returned was lost"
                );
            }
        }
    }
}

#[test]
#[ignore = "left out of mise run check, run by mise run random"]
fn an_export_stays_whole_while_the_source_rewrites_and_compacts() {
    let fs = SimFs::new();
    let busy = EngineOptions {
        checkpoint: CheckpointMode::Background,
        memtable_limit: 40_000,
        wal_limit: 200_000,
        segment_target: 64 << 10,
        reclaim_slack: 0,
        ..options()
    };
    let source = open(&fs, SOURCE, busy).unwrap();
    for round in 0..20 {
        write_round(&source, round, 80);
    }
    source.compact().unwrap();
    let expected = dump(&source);
    let export = source.export(&[]).unwrap();
    let pinned: Vec<_> = source.extents().entries.into_keys().collect();
    let rounds = u64::from(test_scale::cases(60).min(90));
    let first = std::thread::scope(|scope| {
        let writer = scope.spawn(|| {
            for round in 20..20 + rounds {
                write_round(&source, round, 80);
                let mut delete = source.begin_write().unwrap();
                delete
                    .delete_range(Table::PropertyEntries, &[], &[0xff; 9])
                    .unwrap();
                delete.commit().unwrap();
                if round % 7 == 0 {
                    source.compact().unwrap();
                }
            }
        });
        let mut bytes = Vec::new();
        let mut buffer = vec![0; 40_000];
        while !writer.is_finished() || (bytes.len() as u64) < export.len() {
            let read = export.read_at(bytes.len() as u64, &mut buffer).unwrap();
            bytes.extend_from_slice(&buffer[..read]);
            std::thread::yield_now();
        }
        writer.join().unwrap();
        bytes
    });
    source.compact().unwrap();
    assert_eq!(read_all(&export, 1 << 16), first);
    let installed = install_bytes(&SimFs::new(), &first).unwrap();
    installed.check().unwrap();
    assert_eq!(dump(&installed), expected);
    drop(export);
    write_round(&source, 1000, 5);
    source.compact().unwrap();
    let after: Vec<_> = source.extents().entries.into_keys().collect();
    assert!(
        after.iter().all(|id| !pinned.contains(id)),
        "the source never moved off the pinned extents, so nothing tried to reuse them"
    );
}

fn retaining(max_retained_bytes: u64) -> EngineOptions {
    EngineOptions {
        segment_target: 4096,
        memtable_limit: 20_000,
        wal_limit: 16_384,
        max_retained_bytes,
        ..options()
    }
}

fn small_commit(engine: &impl Storage, number: u64) {
    let mut write = engine.begin_write().unwrap();
    write
        .put(
            Table::Metadata,
            &(number % 50).to_be_bytes(),
            &[number as u8; 900],
        )
        .unwrap();
    write.commit().unwrap();
}

#[test]
fn an_acknowledged_peer_keeps_the_log_after_its_position_and_releases_it_as_it_moves() {
    let fs = SimFs::new();
    let engine = open(&fs, SOURCE, retaining(u64::MAX)).unwrap();
    let unpinned = open(&SimFs::new(), SOURCE, retaining(u64::MAX)).unwrap();
    for number in 1..=5 {
        small_commit(&engine, number);
        small_commit(&unpinned, number);
    }
    engine.acknowledge(b"peer", 5).unwrap();
    for number in 6..=300 {
        small_commit(&engine, number);
        small_commit(&unpinned, number);
        assert!(
            engine.log_start() <= 6,
            "commit {number} dropped the peer's log"
        );
    }
    assert!(unpinned.log_start() > 200, "the log of no peer was kept");
    engine.acknowledge(b"peer", 250).unwrap();
    small_commit(&engine, 301);
    let start = engine.log_start();
    assert!((200..=251).contains(&start), "the log starts at {start}");
    assert!(engine.forget(b"peer"));
    assert!(!engine.forget(b"peer"));
    for number in 302..=400 {
        small_commit(&engine, number);
    }
    assert!(engine.log_start() > 300, "a forgotten peer kept the log");
}

#[test]
fn the_cap_releases_a_peer_that_falls_too_far_behind_and_bounds_the_log() {
    let cap = 64 << 10;
    let options = retaining(cap);
    let engine = open(&SimFs::new(), SOURCE, options).unwrap();
    small_commit(&engine, 1);
    engine.acknowledge(b"slow", 1).unwrap();
    engine.acknowledge(b"fast", 1).unwrap();
    let bound = cap + options.wal_limit + 4 * options.segment_target + 8192;
    for number in 2..=600 {
        small_commit(&engine, number);
        if number % 10 == 0 {
            engine.acknowledge(b"fast", number).unwrap();
        }
        let log = engine.stats().unwrap().wal_bytes;
        assert!(log <= bound, "commit {number} left {log} bytes of log");
    }
    assert!(engine.log_start() > 2, "the cap kept the slow peer's log");
    assert!(
        engine.log_start() <= 591,
        "the cap released the peer within it"
    );
    engine.acknowledge(b"slow", 600).unwrap();
    small_commit(&engine, 601);
    assert!(engine.log_start() <= 601);
}

#[test]
fn an_export_keeps_the_log_after_its_position_until_it_is_dropped() {
    let engine = open(&SimFs::new(), SOURCE, retaining(u64::MAX)).unwrap();
    for number in 1..=40 {
        small_commit(&engine, number);
    }
    let export = engine.export(&[]).unwrap();
    let position = export.position();
    assert!(position > 0);
    for number in 41..=300 {
        small_commit(&engine, number);
    }
    assert!(engine.log_start() <= position + 1);
    drop(export);
    small_commit(&engine, 301);
    assert!(engine.log_start() > position + 1);
}

#[test]
fn an_export_keeps_its_log_past_the_peer_cap_until_its_time_limit_releases_it() {
    let options = EngineOptions {
        pin_limit: std::time::Duration::from_secs(3600),
        ..retaining(0)
    };
    let engine = open(&SimFs::new(), SOURCE, options).unwrap();
    for number in 1..=40 {
        small_commit(&engine, number);
    }
    let export = engine.export(&[]).unwrap();
    let position = export.position();
    let mut first = vec![0; 100];
    assert_eq!(export.read_at(0, &mut first).unwrap(), 100);
    for number in 41..=200 {
        small_commit(&engine, number);
    }
    assert!(
        engine.log_start() <= position + 1,
        "the peer cap released an export"
    );
    assert!(engine.stats().unwrap().held_bytes > 0);
    engine.advance_clock(std::time::Duration::from_secs(3601));
    for number in 201..=300 {
        small_commit(&engine, number);
    }
    assert!(
        engine.log_start() > position + 1,
        "an expired export kept the log"
    );
    assert_eq!(engine.stats().unwrap().held_bytes, 0);
    let error = StorageError::from(export.read_at(0, &mut first).unwrap_err());
    assert_eq!(error.kind(), ErrorKind::SnapshotExpired);
}

#[test]
fn a_log_kept_for_a_peer_past_the_checkpoint_limit_starts_no_extra_checkpoints() {
    let options = EngineOptions {
        memtable_limit: u64::MAX,
        ..retaining(u64::MAX)
    };
    let engine = open(&SimFs::new(), SOURCE, options).unwrap();
    small_commit(&engine, 1);
    engine.acknowledge(b"lagging", 1).unwrap();
    let before = engine.header().generation;
    for number in 2..=600 {
        small_commit(&engine, number);
    }
    let checkpoints = engine.header().generation - before;
    let log = engine.stats().unwrap().wal_bytes;
    assert!(
        log > 10 * options.wal_limit,
        "only {log} bytes of log were kept"
    );
    let expected = log / options.wal_limit;
    assert!(
        (expected / 2..=expected * 2 + 4).contains(&checkpoints),
        "{checkpoints} checkpoints for {log} bytes of log against a limit of {}",
        options.wal_limit
    );
    assert!(engine.log_start() <= 2);
}

#[test]
#[ignore = "left out of mise run check, run by mise run random"]
fn a_snapshot_taken_during_staged_checkpoints_and_compaction_holds_the_state_at_its_position() {
    let rounds = 20 + u64::from(test_scale::cases(40).min(400));
    let reference = open(&SimFs::new(), SOURCE, by_hand()).unwrap();
    let mut states = vec![digest(&State::new())];
    for round in 0..rounds {
        write_round(&reference, round, 60);
        states.push(digest(&dump(&reference)));
    }
    let busy = EngineOptions {
        checkpoint: CheckpointMode::Background,
        memtable_limit: 30_000,
        wal_limit: 150_000,
        segment_target: 64 << 10,
        stage_floor: 4,
        ..options()
    };
    let source = open(&SimFs::new(), SOURCE, busy).unwrap();
    let mut checked = std::collections::BTreeSet::new();
    std::thread::scope(|scope| {
        let writer = scope.spawn(|| {
            for round in 0..rounds {
                write_round(&source, round, 60);
                if round % 11 == 10 {
                    source.compact().unwrap();
                }
            }
        });
        while !writer.is_finished() {
            let export = source.export(&[]).unwrap();
            let position = export.position();
            let bytes = read_all(&export, 1 << 16);
            drop(export);
            let installed = install_bytes(&SimFs::new(), &bytes).unwrap();
            installed.check().unwrap();
            assert!(
                digest(&dump(&installed)) == states[position as usize],
                "the snapshot at {position} holds another state"
            );
            checked.insert(position);
        }
        writer.join().unwrap();
    });
    assert!(
        checked.len() >= 3,
        "only positions {checked:?} were checked"
    );
}

fn segment_log(fs: &SimFs, name: &str) -> Vec<(u128, u64)> {
    let block = crate::engine::block_file::BlockSize::new(WAL_BLOCK).unwrap();
    let mut records = Vec::new();
    for sequence in segment_files(fs, name).unwrap() {
        let file = fs.open(&segment_name(name, sequence), block).unwrap();
        let mut bytes = vec![0; file.block_count().unwrap() as usize * WAL_BLOCK];
        file.read(0, &mut bytes).unwrap();
        let Ok(header) = SegmentHeader::decode(&bytes[..WAL_BLOCK]) else {
            continue;
        };
        let mut at = header.unit();
        while let Ok((record, length)) = decode_record(header.record_seed(), &bytes[at..]) {
            records.push((header.database_id, record.lsn));
            at += padded(length, header.unit());
        }
    }
    records
}

fn readable_from(fs: &SimFs, name: &str) -> u64 {
    let records = segment_log(fs, name);
    let lsns: Vec<u64> = records.iter().map(|(_, lsn)| *lsn).collect();
    for pair in lsns.windows(2) {
        assert_eq!(
            pair[0] + 1,
            pair[1],
            "the kept log skips from {} to {}",
            pair[0],
            pair[1]
        );
    }
    lsns.first().copied().unwrap_or(u64::MAX)
}

#[test]
fn a_peer_and_the_log_kept_for_it_survive_a_clean_restart_and_a_crash() {
    let fs = SimFs::new();
    let engine = open(&fs, SOURCE, retaining(u64::MAX)).unwrap();
    for number in 1..=5 {
        small_commit(&engine, number);
    }
    engine.acknowledge(b"peer", 5).unwrap();
    for number in 6..=300 {
        small_commit(&engine, number);
    }
    drop(engine);
    assert!(
        readable_from(&fs, SOURCE) <= 6,
        "a clean close dropped the peer's log"
    );
    let engine = open(&fs, SOURCE, retaining(u64::MAX)).unwrap();
    assert!(
        engine.log_start() <= 6,
        "a clean open dropped the peer's log"
    );
    for number in 301..=400 {
        small_commit(&engine, number);
    }
    assert!(readable_from(&fs, SOURCE) <= 6);
    engine.acknowledge(b"peer", 350).unwrap();
    for number in 401..=450 {
        small_commit(&engine, number);
    }
    let crashed = fs.crash(Crash::DropUnsynced);
    drop(engine);
    let engine = open(&crashed, SOURCE, retaining(u64::MAX)).unwrap();
    let start = engine.log_start();
    assert!(
        start <= 351,
        "a crash dropped the peer's log, which starts at {start}"
    );
    assert!(readable_from(&crashed, SOURCE) <= 351);
    assert!(
        engine.forget(b"peer"),
        "the peer was not kept across the crash"
    );
    for number in 451..=600 {
        small_commit(&engine, number);
    }
    assert!(engine.log_start() > 450, "a forgotten peer kept the log");
    drop(engine);
    assert!(readable_from(&crashed, SOURCE) > 450);
}

#[test]
fn damage_in_the_log_kept_only_for_peers_shortens_it_and_opens() {
    let fs = SimFs::new();
    let engine = open(&fs, SOURCE, retaining(u64::MAX)).unwrap();
    small_commit(&engine, 1);
    engine.acknowledge(b"peer", 1).unwrap();
    for number in 2..=300 {
        small_commit(&engine, number);
    }
    drop(engine);
    let sequences = segment_files(&fs, SOURCE).unwrap();
    let damaged = sequences[sequences.len() / 3];
    let block = crate::engine::block_file::BlockSize::new(WAL_BLOCK).unwrap();
    fs.open(&segment_name(SOURCE, damaged), block)
        .unwrap()
        .write(0, &[0x5a; WAL_BLOCK])
        .unwrap();
    let engine = open(&fs, SOURCE, retaining(u64::MAX)).unwrap();
    let start = engine.log_start();
    assert!(start > 2, "the damaged segment was kept");
    assert_eq!(readable_from(&fs, SOURCE), start);
    assert!(
        segment_files(&fs, SOURCE)
            .unwrap()
            .iter()
            .all(|sequence| *sequence > damaged)
    );
    small_commit(&engine, 301);
    assert_eq!(dump(&engine).len(), 50);
}

#[test]
fn an_install_over_a_database_with_a_kept_log_leaves_none_of_it_to_serve() {
    let (_fs, source, expected) = loaded_source(3, 20);
    let bytes = read_all(&source.export(&[]).unwrap(), 1 << 16);
    let base = SimFs::new();
    let old = open(&base, TARGET, retaining(u64::MAX)).unwrap();
    small_commit(&old, 1);
    old.acknowledge(b"peer", 1).unwrap();
    for number in 2..=200 {
        small_commit(&old, number);
    }
    let old_state = dump(&old);
    drop(old);
    assert!(!segment_log(&base, TARGET).is_empty());
    let total = {
        let copy = base.crash(Crash::DropUnsynced);
        let before = copy.operations();
        drop(install_bytes(&copy, &bytes).unwrap());
        copy.operations() - before
    };
    for fail_at in 0..=total {
        let copy = base.crash(Crash::DropUnsynced);
        copy.fail_after(fail_at, Failure::FromThenOn);
        let _ = install_bytes(&copy, &bytes);
        let crashed = copy.crash(Crash::DropUnsynced);
        let reopened = open(&crashed, TARGET, options()).unwrap();
        let found = dump(&reopened);
        if found == expected {
            let id = reopened.header().database_id;
            assert!(
                segment_log(&crashed, TARGET)
                    .iter()
                    .all(|(owner, _)| *owner == id),
                "fail at {fail_at}: the old database's log is still there to serve"
            );
        } else {
            assert_eq!(found, old_state, "fail at {fail_at}");
        }
    }
}

#[test]
fn installing_over_a_database_open_on_the_simulated_file_system_fails_and_keeps_it() {
    let (_fs, source, _) = loaded_source(3, 10);
    let bytes = read_all(&source.export(&[]).unwrap(), 1 << 16);
    let target = SimFs::new();
    let held = open(&target, TARGET, by_hand()).unwrap();
    write_round(&held, 1, 5);
    let before = dump(&held);
    let error = install_bytes(&target, &bytes).unwrap_err();
    assert_eq!(
        StorageError::from(error).kind(),
        ErrorKind::DatabaseAlreadyOpen
    );
    assert_eq!(dump(&held), before);
    write_round(&held, 2, 5);
    drop(held);
    assert_eq!(
        dump(&install_bytes(&target, &bytes).unwrap()).len(),
        dump(&source).len()
    );
}

#[test]
fn an_install_refuses_a_snapshot_older_than_the_terms_the_locked_database_began() {
    let source_fs = SimFs::new();
    let source = open(&source_fs, SOURCE, by_hand()).unwrap();
    source.promote(1, b"node-a").unwrap();
    write_round(&source, 1, 10);
    source.checkpoint().unwrap();
    let old = read_all(&source.export(&[]).unwrap(), 1 << 16);
    let target = SimFs::new();
    let replica = install(
        Arc::new(target.clone()),
        TARGET,
        old.as_slice(),
        options(),
        false,
    )
    .unwrap();
    replica.promote(4, b"node-b").unwrap();
    replica.checkpoint().unwrap();
    drop(replica);
    let error = install(
        Arc::new(target.clone()),
        TARGET,
        old.as_slice(),
        options(),
        false,
    )
    .unwrap_err();
    assert_eq!(StorageError::from(error).kind(), ErrorKind::Fenced);
}

#[test]
fn installing_over_another_database_fails_unless_forced_and_keeps_it() {
    let (_fs, source, expected) = loaded_source(3, 10);
    let bytes = read_all(&source.export(&[]).unwrap(), 1 << 16);
    let (target, old) = old_target();
    let error = install(
        Arc::new(target.clone()),
        TARGET,
        bytes.as_slice(),
        options(),
        false,
    )
    .unwrap_err();
    assert_eq!(
        StorageError::from(error).kind(),
        ErrorKind::DifferentDatabase
    );
    assert_eq!(dump(&open(&target, TARGET, options()).unwrap()), old);
    let installed = install(
        Arc::new(target.clone()),
        TARGET,
        bytes.as_slice(),
        options(),
        true,
    )
    .unwrap();
    assert_eq!(dump(&installed), expected);
    drop(installed);
    let again = install(
        Arc::new(target.clone()),
        TARGET,
        bytes.as_slice(),
        options(),
        false,
    );
    assert_eq!(
        dump(&again.unwrap()),
        expected,
        "a snapshot of the same database was refused"
    );
}
