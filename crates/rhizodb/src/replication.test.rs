use proptest::prelude::*;

use super::Position;
use crate::error::ErrorKind;

proptest! {
    #[test]
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
    use crate::graph::Graph;
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
            cache_pages: 64,
            reclaim_slack: 64 << 10,
            stage_floor: 8,
            ..EngineOptions::default()
        }
    }

    pub(super) fn open_replica(fs: &SimFs) -> Result<Database, crate::Error> {
        let engine = Engine::open(Arc::new(AnyFs::Sim(fs.clone())), NAME, options())
            .map_err(crate::storage::StorageError::from)?;
        Ok(Database::from_parts(
            Graph::open_as(engine, true)?,
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
                writer.demote().unwrap();
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
}
