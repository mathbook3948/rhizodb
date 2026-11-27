use proptest::prelude::*;

use super::Position;
use crate::error::ErrorKind;

proptest! {
    #[test]
    #[ignore = "left out of mise run check, run by mise run random"]
    fn a_position_reads_back_from_its_bytes(
        term in 1u64..,
        writer_id in proptest::collection::vec(any::<u8>(), 1..=64),
        lsn in any::<u64>(),
    ) {
        let position = Position::new(term, &writer_id, lsn).map_err(|error| TestCaseError::fail(error.to_string()))?;
        let read = Position::from_bytes(&position.to_bytes()).map_err(|error| TestCaseError::fail(error.to_string()))?;
        prop_assert_eq!(read, position);
    }

    #[test]
    #[ignore = "left out of mise run check, run by mise run random"]
    fn arbitrary_bytes_fail_as_corrupt_data_or_read_back_the_same(bytes in proptest::collection::vec(any::<u8>(), 0..100)) {
        match Position::from_bytes(&bytes) {
            Ok(position) => prop_assert_eq!(position.to_bytes(), bytes),
            Err(error) => prop_assert_eq!(error.kind(), ErrorKind::CorruptData),
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(crate::test_scale::cases(48)))]

    #[test]
    #[ignore = "left out of mise run check, run by mise run random"]
    fn damaged_changes_fail_without_a_panic(edits in proptest::collection::vec(any::<u8>(), 0..64)) {
        super::apply_changes_for_fuzzing(&edits).map_err(TestCaseError::fail)?;
    }
}

#[test]
fn only_term_zero_has_no_writer_id() {
    assert!(Position::new(0, b"", 7).is_ok());
    for (term, writer_id) in [(0, &b"a"[..]), (1, b""), (1, &[0; 65])] {
        let error = Position::new(term, writer_id, 1).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidArgumentValue);
    }
}

mod crash {
    use std::collections::HashMap;
    use std::sync::Arc;

    use super::Position;
    use crate::database::Database;
    use crate::engine::any_fs::AnyFs;
    use crate::engine::sim_fs::Crash;
    use crate::engine::sim_fs::Failure;
    use crate::engine::sim_fs::SimFs;
    use crate::engine::store::CheckpointMode;
    use crate::engine::store::Engine;
    use crate::engine::store::EngineOptions;
    use crate::error::ErrorKind;
    use crate::graph::IndexTarget;
    use crate::query::Statements;
    use crate::replication::Changes;
    use crate::result::Parameters;
    use crate::result::Value;
    use crate::test_scale;

    const NAME: &str = "replica.rhizodb";

    fn options() -> EngineOptions {
        EngineOptions {
            segment_target: 4096,
            memtable_limit: 4_000,
            wal_limit: 12_000,
            checkpoint: CheckpointMode::Inline,
            cache_bytes: 64 * crate::engine::layout::BLOCK,
            reclaim_slack: 64 << 10,
            stage_floor: 8,
            ..EngineOptions::default()
        }
    }

    pub(super) fn open_replica(fs: &SimFs) -> Result<Database, crate::Error> {
        let engine = Engine::open(Arc::new(AnyFs::Sim(fs.clone())), NAME, options())
            .map_err(crate::storage::StorageError::from)?;
        Ok(Database::from_parts(
            crate::database::graph_on(engine, true)?,
            Statements::default(),
        ))
    }

    fn parameters(p: i64) -> Parameters {
        Parameters::from([("p".to_owned(), Value::Integer(p))])
    }

    fn export(db: &Database) -> Vec<u8> {
        let mut out = Vec::new();
        db.export(&mut out).unwrap();
        out
    }

    /// A writer that ran a workload of writes and index changes, with the export it held at
    /// each position it passed.
    pub(super) fn written() -> (Database, HashMap<Position, Vec<u8>>) {
        let writer = Database::in_memory().unwrap();
        let mut seen = HashMap::new();
        seen.insert(Position::start(), export(&Database::in_memory().unwrap()));
        let mut note = |db: &Database| {
            seen.insert(db.position(), export(db));
        };
        note(&writer);
        writer.promote(1, b"first").unwrap();
        note(&writer);
        writer
            .create_property_index("by_k", IndexTarget::Label("Item"), "k")
            .unwrap();
        note(&writer);
        for p in 0..6 {
            writer
                .execute(
                    "UNWIND range($p * 10, $p * 10 + 9) AS i \
                     INSERT (:Item {k: i, text: 'word ' + toString(i)})-[:R {w: i}]->(:Other)",
                    &parameters(p),
                )
                .unwrap();
            note(&writer);
            if p == 2 {
                writer
                    .create_keyword_index("text", IndexTarget::Label("Item"), &["text"])
                    .unwrap();
                note(&writer);
            }
            if p == 4 {
                writer.demote(std::time::Duration::from_secs(5)).unwrap();
                note(&writer);
                writer.promote(2, b"second").unwrap();
                note(&writer);
            }
            writer
                .execute(
                    "MATCH (n:Item) WHERE n.k % 3 = $p % 3 SET n.text = 'graph', n.tag = $p",
                    &parameters(p),
                )
                .unwrap();
            note(&writer);
        }
        writer
            .execute(
                "MATCH (n:Item) WHERE n.k % 4 = 0 DETACH DELETE n",
                &Parameters::new(),
            )
            .unwrap();
        note(&writer);
        writer.drop_index("text").unwrap();
        note(&writer);
        (writer, seen)
    }

    fn batches(writer: &Database) -> Vec<Changes> {
        let mut position = Position::start();
        let mut batches = Vec::new();
        loop {
            let changes = writer.changes_since(&position, 2048).unwrap();
            if changes.is_empty() {
                return batches;
            }
            position = changes.last().clone();
            batches.push(changes);
        }
    }

    fn apply_all(db: &Database, batches: &[Changes]) -> Result<(), crate::Error> {
        for changes in batches {
            db.apply(changes)?;
        }
        Ok(())
    }

    fn integrity(db: &Database) {
        db.check().unwrap();
        if db.position().lsn() == 0 {
            return;
        }
        let violations = db
            .graph
            .read()
            .and_then(|read| read.check_integrity())
            .unwrap();
        assert!(violations.is_empty(), "{violations:?}");
    }

    #[test]
    #[ignore = "left out of mise run check, run by mise run random"]
    fn a_crash_at_any_write_of_apply_leaves_a_position_the_writer_passed() {
        let (writer, seen) = written();
        let batches = batches(&writer);
        let fs = SimFs::new();
        let replica = open_replica(&fs).unwrap();
        let before = fs.operations();
        apply_all(&replica, &batches).unwrap();
        let operations = fs.operations() - before;
        assert_eq!(replica.position(), writer.position());
        assert_eq!(export(&replica), export(&writer));
        drop(replica);
        let step = (operations / u64::from(test_scale::cases(60))).max(1);
        let mut crashes = 0;
        for failing in (0..operations).step_by(step as usize) {
            let fs = SimFs::new();
            let replica = open_replica(&fs).unwrap();
            fs.fail_after(failing, Failure::FromThenOn);
            if let Err(error) = apply_all(&replica, &batches) {
                assert!(
                    matches!(error.kind(), ErrorKind::Storage | ErrorKind::CorruptData),
                    "{error}"
                );
            }
            drop(replica);
            for crash in [
                Crash::DropUnsynced,
                Crash::KeepUnsynced,
                Crash::Reorder { seed: failing },
                Crash::Scramble { seed: failing },
            ] {
                let recovered = fs.crash(crash);
                let replica = open_replica(&recovered).unwrap();
                let position = replica.position();
                let expected = seen
                    .get(&position)
                    .unwrap_or_else(|| panic!("{position:?} after {crash:?} at {failing}"));
                assert_eq!(&export(&replica), expected, "{crash:?} at {failing}");
                integrity(&replica);
                apply_all(&replica, &batches).unwrap();
                assert_eq!(replica.position(), writer.position());
                assert_eq!(export(&replica), export(&writer));
                drop(replica);
                let reopened = open_replica(&recovered).unwrap();
                assert_eq!(reopened.position(), writer.position());
                integrity(&reopened);
                crashes += 1;
            }
        }
        assert!(crashes > 0);
    }

    fn open_on(fs: &SimFs, replica: bool) -> Database {
        let roomy = EngineOptions {
            memtable_limit: 1 << 30,
            wal_limit: 1 << 40,
            ..options()
        };
        let engine = Engine::open(Arc::new(AnyFs::Sim(fs.clone())), NAME, roomy)
            .map_err(crate::storage::StorageError::from)
            .unwrap();
        Database::from_parts(
            crate::database::graph_on(engine, replica).unwrap(),
            Statements::default(),
        )
    }

    fn sync(from: &Database, to: &Database) -> Result<Position, crate::Error> {
        loop {
            let changes = from.changes_since(&to.position(), 1 << 14)?;
            if changes.is_empty() {
                return Ok(to.position());
            }
            to.apply(&changes)?;
        }
    }

    fn insert(db: &Database, k: i64) {
        db.execute("INSERT (:N {k: $p})", &parameters(k)).unwrap();
    }

    #[test]
    fn a_killed_writer_and_relay_keep_serving_their_lagging_replicas() {
        let writer_fs = SimFs::new();
        let relay_fs = SimFs::new();
        let writer = open_on(&writer_fs, false);
        writer.promote(1, b"w").unwrap();
        let relay = open_on(&relay_fs, true);
        let lagging = open_on(&SimFs::new(), true);
        for k in 0..20 {
            insert(&writer, k);
        }
        sync(&writer, &relay).unwrap();
        sync(&relay, &lagging).unwrap();
        for k in 20..40 {
            insert(&writer, k);
        }
        sync(&writer, &relay).unwrap();
        let writer_fs = writer_fs.crash(Crash::DropUnsynced);
        let relay_fs = relay_fs.crash(Crash::Reorder { seed: 7 });
        drop(writer);
        drop(relay);
        let writer = open_on(&writer_fs, false);
        let relay = open_on(&relay_fs, false);
        assert_eq!(writer.role(), crate::replication::Role::Fenced);
        assert_eq!(relay.role(), crate::replication::Role::Replica);
        sync(&relay, &lagging).unwrap();
        writer.promote(1, b"w").unwrap();
        insert(&writer, 40);
        sync(&writer, &lagging).unwrap();
        sync(&writer, &relay).unwrap();
        assert_eq!(lagging.position(), writer.position());
        assert_eq!(relay.position(), writer.position());
        assert_eq!(export(&lagging), export(&writer));
    }

    #[test]
    fn a_log_kept_through_a_recovery_outlives_a_header_write_and_a_second_crash() {
        let fs = SimFs::new();
        let writer = open_on(&fs, false);
        writer.promote(1, b"w").unwrap();
        for k in 0..10 {
            insert(&writer, k);
        }
        let fs = fs.crash(Crash::KeepUnsynced);
        drop(writer);
        let writer = open_on(&fs, false);
        writer.promote(1, b"w").unwrap();
        insert(&writer, 10);
        let ahead = Position::new(2, b"x", writer.position().lsn()).unwrap();
        let _ = writer.changes_since(&ahead, 1 << 20);
        assert_eq!(writer.role(), crate::replication::Role::Fenced);
        let fs = fs.crash(Crash::KeepUnsynced);
        drop(writer);
        let writer = open_on(&fs, false);
        let fresh = open_on(&SimFs::new(), true);
        assert_eq!(sync(&writer, &fresh).unwrap(), writer.position());
        assert_eq!(export(&fresh), export(&writer));
    }

    fn snapshot_bytes(db: &Database) -> Vec<u8> {
        let snapshot = db.snapshot().unwrap();
        let mut bytes = vec![0; snapshot.len() as usize];
        let mut at = 0;
        while at < bytes.len() {
            at += snapshot.read_at(at as u64, &mut bytes[at..]).unwrap();
        }
        bytes
    }

    fn install_on(fs: &SimFs, bytes: &[u8]) -> Result<Database, crate::Error> {
        let roomy = EngineOptions {
            memtable_limit: 1 << 30,
            wal_limit: 1 << 40,
            ..options()
        };
        let engine = crate::engine::snapshot::install(
            Arc::new(AnyFs::Sim(fs.clone())),
            NAME,
            bytes,
            roomy,
            false,
        )
        .map_err(crate::storage::StorageError::from)?;
        Ok(Database::from_parts(
            crate::database::graph_on(engine, true)?,
            Statements::default(),
        ))
    }

    #[test]
    fn an_install_after_a_crash_refuses_a_snapshot_older_than_a_term_only_the_log_holds() {
        let writer = open_on(&SimFs::new(), false);
        writer.promote(3, b"a").unwrap();
        insert(&writer, 1);
        writer.compact().unwrap();
        let old = snapshot_bytes(&writer);
        let target = SimFs::new();
        let replica = install_on(&target, &old).unwrap();
        let next = install_on(&SimFs::new(), &old).unwrap();
        next.promote(5, b"b").unwrap();
        insert(&next, 2);
        sync(&next, &replica).unwrap();
        assert_eq!(replica.position().term(), 5);
        let crashed = target.crash(Crash::KeepUnsynced);
        drop(replica);
        let error = install_on(&crashed, &old).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Fenced);
    }

    #[test]
    fn an_install_over_a_log_damaged_before_its_end_fails_unless_forced() {
        use crate::engine::block_file::BlockFile;
        use crate::engine::block_file::BlockSize;
        use crate::engine::block_file::FileSystem;
        let writer = open_on(&SimFs::new(), false);
        writer.promote(3, b"a").unwrap();
        insert(&writer, 1);
        writer.compact().unwrap();
        let old = snapshot_bytes(&writer);
        let target = SimFs::new();
        let replica = install_on(&target, &old).unwrap();
        let next = install_on(&SimFs::new(), &old).unwrap();
        next.promote(5, b"b").unwrap();
        for k in 2..6 {
            insert(&next, k);
        }
        sync(&next, &replica).unwrap();
        let crashed = target.crash(Crash::KeepUnsynced);
        drop(replica);
        let unit = BlockSize::new(crate::engine::layout::WAL_BLOCK).unwrap();
        let first_record = (4096 / crate::engine::layout::WAL_BLOCK) as u64;
        let mut damaged = 0;
        for name in crashed.names().unwrap() {
            if name == NAME || !name.starts_with(NAME) {
                continue;
            }
            let segment = crashed.open(&name, unit).unwrap();
            if segment.block_count().unwrap() <= first_record {
                continue;
            }
            let mut block = vec![0; crate::engine::layout::WAL_BLOCK];
            segment.read(first_record, &mut block).unwrap();
            block[30] ^= 0xFF;
            segment.write(first_record, &block).unwrap();
            segment.sync().unwrap();
            damaged += 1;
        }
        assert!(damaged > 0);
        let error = install_on(&crashed, &old).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::CorruptData);
        assert!(error.to_string().contains("force"), "{error}");
        let engine = crate::engine::snapshot::install(
            Arc::new(AnyFs::Sim(crashed.clone())),
            NAME,
            old.as_slice(),
            options(),
            true,
        )
        .unwrap();
        drop(engine);
    }

    fn while_checkpointing<T>(db: &Database, run: impl FnOnce() -> T) -> T {
        let (locked, is_locked) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel::<()>();
        std::thread::scope(|scope| {
            scope.spawn(move || {
                db.graph.storage().while_checkpointing(|| {
                    locked.send(()).unwrap();
                    let _ = released.recv_timeout(std::time::Duration::from_secs(2));
                });
            });
            is_locked.recv().unwrap();
            let start = std::time::Instant::now();
            let out = run();
            let waited = start.elapsed();
            let _ = release.send(());
            assert!(
                waited < std::time::Duration::from_secs(1),
                "a replication call waited {waited:?} on a checkpoint"
            );
            out
        })
    }

    #[test]
    fn positions_changes_and_applies_never_wait_on_a_checkpoint() {
        let writer = open_on(&SimFs::new(), false);
        writer.promote(1, b"w").unwrap();
        let replica = open_on(&SimFs::new(), true);
        insert(&writer, 1);
        sync(&writer, &replica).unwrap();
        insert(&writer, 2);
        let expected = writer.position();
        let changes = while_checkpointing(&writer, || {
            assert_eq!(writer.position(), expected);
            writer.changes_since(&replica.position(), 1 << 20).unwrap()
        });
        let reached = while_checkpointing(&replica, || replica.apply(&changes).unwrap());
        assert_eq!(reached, expected);
    }

    #[test]
    fn a_checkpoint_that_keeps_the_log_and_a_crash_leave_every_change_readable() {
        let fs = SimFs::new();
        let writer = open_on(&fs, false);
        writer.promote(1, b"w").unwrap();
        for k in 0..20 {
            insert(&writer, k);
        }
        writer.graph.storage().checkpoint_keeping_log().unwrap();
        let crashed = fs.crash(Crash::KeepUnsynced);
        drop(writer);
        let writer = open_on(&crashed, false);
        let fresh = open_on(&SimFs::new(), true);
        assert_eq!(sync(&writer, &fresh).unwrap(), writer.position());
        assert_eq!(export(&fresh), export(&writer));
    }

    #[test]
    fn the_checkpoint_of_an_upgrade_keeps_the_log_for_replicas_behind_it() {
        use crate::storage::Storage;
        use crate::storage::Table;
        use crate::storage::WriteTransaction;
        let fs = SimFs::new();
        let writer = open_on(&fs, false);
        for k in 0..20 {
            insert(&writer, k);
        }
        let mut write = writer.graph.storage().begin_write().unwrap();
        write
            .put(Table::Metadata, b"format_version", &13u32.to_be_bytes())
            .unwrap();
        write.commit().unwrap();
        let crashed = fs.crash(Crash::KeepUnsynced);
        drop(writer);
        let writer = open_on(&crashed, false);
        assert_eq!(writer.role(), crate::replication::Role::Writer);
        let fresh = open_on(&SimFs::new(), true);
        assert_eq!(sync(&writer, &fresh).unwrap(), writer.position());
        assert_eq!(export(&fresh), export(&writer));
    }

    #[test]
    fn a_stale_writers_tail_relayed_to_a_replica_diverges_from_the_new_lineage() {
        let old = open_on(&SimFs::new(), false);
        old.promote(5, b"a").unwrap();
        insert(&old, 1);
        let next = open_on(&SimFs::new(), true);
        let follower = open_on(&SimFs::new(), true);
        sync(&old, &next).unwrap();
        sync(&old, &follower).unwrap();
        next.promote(6, b"x").unwrap();
        insert(&next, 2);
        for k in 3..8 {
            insert(&old, k);
        }
        sync(&old, &follower).unwrap();
        let error = next
            .changes_since(&follower.position(), 1 << 20)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Diverged);
        let fresh = open_on(&SimFs::new(), true);
        sync(&next, &fresh).unwrap();
        let stale = old.changes_since(&Position::start(), 1 << 20);
        if let Ok(stale) = stale {
            assert!(matches!(
                fresh.apply(&stale).unwrap_err().kind(),
                ErrorKind::Fenced | ErrorKind::Diverged
            ));
        }
    }

    #[test]
    fn a_rolled_back_copy_that_reuses_its_epoch_is_told_apart_by_the_chain() {
        let fs = SimFs::new();
        let writer = open_on(&fs, false);
        writer.promote(3, b"w").unwrap();
        insert(&writer, 1);
        let copy_fs = fs.crash(Crash::KeepUnsynced);
        insert(&writer, 2);
        insert(&writer, 3);
        let replica = open_on(&SimFs::new(), true);
        sync(&writer, &replica).unwrap();
        let copy = open_on(&copy_fs, false);
        copy.promote(3, b"w").unwrap();
        insert(&copy, 20);
        insert(&copy, 30);
        assert_eq!(copy.position().lsn(), writer.position().lsn());
        assert_ne!(copy.position(), writer.position());
        let error = copy
            .changes_since(&replica.position(), 1 << 20)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Diverged);
        let behind = open_on(&SimFs::new(), true);
        sync(&copy, &behind).unwrap();
        let error = sync(&writer, &behind).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Diverged);
    }

    #[test]
    #[ignore = "left out of mise run check, run by mise run random"]
    fn a_crash_at_any_write_of_a_promotion_or_demotion_leaves_one_role() {
        let base = SimFs::new();
        let db = open_on(&base, false);
        db.promote(1, b"w").unwrap();
        insert(&db, 1);
        drop(db);
        let probe = base.crash(Crash::KeepUnsynced);
        let db = open_on(&probe, false);
        let before = probe.operations();
        db.promote(1, b"w").unwrap();
        db.demote(std::time::Duration::from_secs(1)).unwrap();
        db.promote(2, b"w").unwrap();
        let operations = probe.operations() - before;
        drop(db);
        for failing in 0..operations {
            for crash in [Crash::DropUnsynced, Crash::Scramble { seed: failing }] {
                let fs = base.crash(Crash::KeepUnsynced);
                let db = open_on(&fs, false);
                fs.fail_after(failing, Failure::FromThenOn);
                let outcome = db
                    .promote(1, b"w")
                    .and_then(|_| db.demote(std::time::Duration::from_secs(1)))
                    .and_then(|_| db.promote(2, b"w"));
                drop(outcome);
                let recovered = fs.crash(crash);
                drop(db);
                let db = open_on(&recovered, false);
                let history = db.history();
                let role = db.role();
                match history
                    .entries()
                    .last()
                    .map(|entry| (entry.term(), entry.end_lsn()))
                {
                    Some((1, None)) | Some((2, None)) => {
                        assert_eq!(role, crate::replication::Role::Fenced, "{failing}")
                    }
                    Some((1, Some(_))) => {
                        assert_eq!(role, crate::replication::Role::Replica, "{failing}")
                    }
                    other => panic!("{other:?} at {failing}"),
                }
                let term = history.entries().last().map_or(1, |entry| entry.term());
                db.promote(term + 1, b"w").unwrap();
                insert(&db, 9);
                integrity(&db);
            }
        }
    }
}
