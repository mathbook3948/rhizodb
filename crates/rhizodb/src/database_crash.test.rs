use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use super::*;
use crate::engine::sim_fs::Crash;
use crate::engine::sim_fs::Failure;
use crate::engine::sim_fs::SimFs;
use crate::result::Value;
use crate::storage::sim_backend;
use crate::test_scale;

#[derive(Clone, Copy, Debug)]
enum Step {
    Query(&'static str, i64),
    Transaction(&'static [&'static str], i64),
    PropertyIndex,
    KeywordIndex,
    VectorIndex,
    DropKeywordIndex,
}

const WORKLOAD: &[Step] = &[
    Step::PropertyIndex,
    Step::Query(
        "UNWIND range($p * 10, $p * 10 + 9) AS i \
         INSERT (:Item {k: i, text: 'word ' + toString(i), vec: [toFloat(i), 1.0]})",
        0,
    ),
    Step::KeywordIndex,
    Step::VectorIndex,
    Step::Query(
        "UNWIND range($p * 10, $p * 10 + 9) AS i \
         INSERT (:Item:Fresh {k: i, text: 'fresh word ' + toString(i), vec: [1.0, toFloat(i)]})",
        1,
    ),
    Step::Query(
        "MATCH (a:Item), (b:Item) WHERE b.k = a.k + 1 AND a.k % 3 = $p \
         INSERT (a)-[:NEXT {w: a.k}]->(b)",
        0,
    ),
    Step::Transaction(
        &[
            "MATCH (n:Item) WHERE n.k % 4 = $p SET n.text = 'graph ' + toString(n.k), n.tag = $p",
            "MATCH (n:Item {k: $p + 5}) DETACH DELETE n",
            "INSERT (:Item {k: 100 + $p, text: 'late graph', vec: [0.5, 0.5]})",
        ],
        1,
    ),
    Step::Query(
        "MATCH (n:Fresh) WHERE n.k % 2 = $p REMOVE n:Fresh SET n:Old",
        0,
    ),
    Step::Query(
        "UNWIND range(1, 40) AS i INSERT (:Bulk {k: $p * 1000 + i, text: 'bulk ' + toString(i), \
         blob: 'x' + toString(i * 7919) + 'yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy'})",
        2,
    ),
    Step::DropKeywordIndex,
    Step::Query(
        "MATCH (n:Item) WHERE n.k < 4 SET n.vec = [toFloat(n.k) + $p, 2.0]",
        3,
    ),
    Step::KeywordIndex,
    Step::Transaction(
        &[
            "MATCH (n:Bulk) WHERE n.k % 5 = $p DELETE n",
            "MATCH ()-[r:NEXT]->() WHERE r.w % 2 = $p DELETE r",
            "MATCH (n:Old) SET n.k = n.k + 1000",
        ],
        0,
    ),
    Step::Query(
        "UNWIND range($p, $p + 30) AS i INSERT (:Item {k: i, text: 'tail word', vec: [1.0, 1.0]})",
        200,
    ),
];

const PROBES: &[&str] = &[
    "MATCH (n) RETURN n ORDER BY id(n)",
    "MATCH (a)-[r]->(b) RETURN id(r), type(r), id(a), id(b), r.w ORDER BY id(r)",
    "MATCH (n:Item {k: 7}) RETURN id(n)",
    "MATCH (n:Item) WHERE n.k >= 3 AND n.k < 15 RETURN id(n), n.k ORDER BY id(n)",
    "CALL db.index.keyword.queryNodes('item_text', 'word graph', 1000) YIELD node, score \
     RETURN id(node), score ORDER BY id(node)",
    "CALL db.index.vector.queryNodes('item_vec', [1.0, 0.25], 1000) YIELD node, score \
     RETURN id(node), score ORDER BY id(node)",
];

fn parameters(p: i64) -> Parameters {
    Parameters::from([("p".to_owned(), Value::Integer(p))])
}

fn apply(db: &Database, step: Step) -> Result<(), Error> {
    match step {
        Step::Query(query, p) => db.execute(query, &parameters(p)).map(drop),
        Step::Transaction(queries, p) => {
            let mut tx = db.begin_write()?;
            for query in queries {
                tx.execute(query, &parameters(p))?;
            }
            tx.commit()
        }
        Step::PropertyIndex => db.create_property_index("item_k", IndexTarget::Label("Item"), "k"),
        Step::KeywordIndex => {
            db.create_keyword_index("item_text", IndexTarget::Label("Item"), &["text"])
        }
        Step::VectorIndex => db.create_vector_index(
            "item_vec",
            IndexTarget::Label("Item"),
            "vec",
            2,
            VectorMetric::Cosine,
        ),
        Step::DropKeywordIndex => db.drop_index("item_text").map(drop),
    }
}

fn dump(db: &Database) -> Vec<String> {
    PROBES
        .iter()
        .map(|probe| match db.execute(probe, &Parameters::new()) {
            Ok(result) => format!("{:?}", result.rows()),
            Err(error) => format!("{:?}", error.kind()),
        })
        .collect()
}

fn on(storage: Result<RedbStorage, crate::storage::StorageError>) -> Result<Database, Error> {
    Ok(Database {
        graph: Graph::open(storage?)?,
        statements: Statements::default(),
    })
}

fn open(fs: &SimFs) -> Result<Database, Error> {
    on(sim_backend::storage(fs))
}

fn consistent(db: &Database) -> Result<(), String> {
    let violations = db
        .graph
        .read()
        .and_then(|read| read.check_integrity())
        .map_err(|error| format!("the integrity check failed: {error}"))?;
    if violations.is_empty() {
        Ok(())
    } else {
        Err(format!("integrity violations: {violations:?}"))
    }
}

#[derive(Default)]
struct Expected {
    dumps: BTreeMap<Vec<usize>, Vec<String>>,
}

impl Expected {
    fn after(&mut self, applied: &[usize]) -> &[String] {
        self.dumps.entry(applied.to_vec()).or_insert_with(|| {
            let db = Database::in_memory().unwrap();
            for &step in applied {
                apply(&db, WORKLOAD[step]).unwrap();
            }
            dump(&db)
        })
    }
}

struct Run {
    acknowledged: Vec<usize>,
    failed: Vec<usize>,
}

fn run_workload(db: &Database, stop_at_first_failure: bool) -> Run {
    let mut run = Run {
        acknowledged: Vec::new(),
        failed: Vec::new(),
    };
    for (index, step) in WORKLOAD.iter().enumerate() {
        match apply(db, *step) {
            Ok(()) => run.acknowledged.push(index),
            Err(error) => {
                assert!(
                    matches!(error.kind(), ErrorKind::Storage | ErrorKind::CorruptData),
                    "step {index} failed with {:?}: {error}",
                    error.kind()
                );
                run.failed.push(index);
                if stop_at_first_failure {
                    break;
                }
            }
        }
    }
    run
}

fn candidates(run: &Run) -> Vec<Vec<usize>> {
    let mut candidates = Vec::new();
    for mask in 0..1_u32 << run.failed.len() {
        let mut steps = run.acknowledged.clone();
        for (bit, failed) in run.failed.iter().enumerate() {
            if mask & (1 << bit) != 0 {
                steps.push(*failed);
            }
        }
        steps.sort_unstable();
        candidates.push(steps);
    }
    candidates
}

fn check_recovered(
    recovered: &SimFs,
    run: &Run,
    expected: &mut Expected,
    case: &str,
) -> Result<(), String> {
    let db = open(recovered).map_err(|error| format!("{case}: reopening failed: {error}"))?;
    consistent(&db).map_err(|problem| format!("{case}: {problem}"))?;
    let found = dump(&db);
    let matched = candidates(run)
        .into_iter()
        .find(|steps| expected.after(steps) == found.as_slice());
    let Some(matched) = matched else {
        return Err(format!(
            "{case}: the recovered graph holds no state of acknowledged steps {:?} with failed steps {:?} applied or not",
            run.acknowledged, run.failed
        ));
    };
    db.execute(
        "INSERT (:After {n: 1})-[:THEN]->(:After {n: 2})",
        &Parameters::new(),
    )
    .map_err(|error| format!("{case}: the recovered database refused a write: {error}"))?;
    consistent(&db).map_err(|problem| format!("{case}: after a write, {problem}"))?;
    drop(db);
    let reopened = open(recovered).map_err(|error| format!("{case}: second reopen: {error}"))?;
    let mut probes = expected.after(&matched).to_vec();
    let after = dump(&reopened);
    probes[0] = after[0].clone();
    probes[1] = after[1].clone();
    if after != probes {
        return Err(format!(
            "{case}: a write after recovery changed the indexed answers"
        ));
    }
    let count = reopened
        .execute("MATCH (n:After) RETURN count(*)", &Parameters::new())
        .map_err(|error| format!("{case}: {error}"))?;
    if count.rows() != [vec![Value::Integer(2)]] {
        return Err(format!("{case}: the write after recovery did not last"));
    }
    Ok(())
}

fn modes(seeds: u64, salt: u64) -> Vec<Crash> {
    let mut modes = vec![Crash::DropUnsynced, Crash::KeepUnsynced];
    modes.extend((0..seeds).map(|seed| Crash::Reorder {
        seed: salt * 1_000 + seed,
    }));
    modes
}

fn operations() -> (u64, u64) {
    let fs = SimFs::new();
    let db = open(&fs).unwrap();
    let opened = fs.operations();
    let run = run_workload(&db, true);
    assert_eq!(run.acknowledged.len(), WORKLOAD.len());
    (opened, fs.operations())
}

fn points(from: u64, total: u64, budget: u64) -> Vec<u64> {
    let span = total - from;
    let stride = (span / budget.max(1)).max(1);
    let mut points: Vec<u64> = (from..total).step_by(stride as usize).collect();
    let mut state = total;
    for _ in 0..budget / 4 {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        points.push(from + (state >> 33) % span);
    }
    points.sort_unstable();
    points.dedup();
    points
}

fn crash_during_workload(failure: Failure) {
    let (opened, total) = operations();
    let budget = u64::from(test_scale::cases(40));
    let seeds = u64::from(test_scale::cases(1)).min(16);
    let mut expected = Expected::default();
    let mut problems = Vec::new();
    for at in points(opened, total, budget) {
        let fs = SimFs::new();
        let run = match open(&fs) {
            Ok(db) => {
                fs.fail_after(at - opened, failure);
                let run = run_workload(&db, failure == Failure::FromThenOn);
                if failure == Failure::Once
                    && let Err(problem) = consistent(&db)
                {
                    problems.push(format!("live after {failure:?} at {at}: {problem}"));
                }
                run
            }
            Err(error) => panic!("opening an empty file system failed: {error}"),
        };
        for mode in modes(seeds, at) {
            let case = format!("{failure:?} at operation {at} of {total}, {mode:?}");
            if let Err(problem) = check_recovered(&fs.crash(mode), &run, &mut expected, &case) {
                problems.push(problem);
            }
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn a_power_loss_at_any_point_of_a_workload_recovers_the_acknowledged_steps() {
    crash_during_workload(Failure::FromThenOn);
}

#[test]
fn one_failed_write_or_sync_in_a_workload_loses_only_its_own_step() {
    crash_during_workload(Failure::Once);
}

#[test]
#[ignore = "bug: a power loss while Database::open creates a new file can leave a file that \
            every later open refuses with a storage error, 'Not a redb database: magic number \
            mismatch', so the host must delete it by hand"]
fn a_power_loss_while_a_new_file_is_created_leaves_a_file_that_opens() {
    let (opened, _) = operations();
    let mut problems = Vec::new();
    for at in 0..opened {
        let fs = SimFs::new();
        fs.fail_after(at, Failure::FromThenOn);
        assert!(open(&fs).is_err(), "opening survived a failure at {at}");
        for mode in modes(2, at) {
            if let Err(error) = open(&fs.crash(mode)) {
                problems.push(format!(
                    "power loss at operation {at} of {opened}, {mode:?}: {error}"
                ));
            }
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn a_full_disk_fails_writes_cleanly_keeps_reads_and_recovers_once_space_returns() {
    let fs = SimFs::new();
    let capacity = Arc::new(AtomicU64::new(u64::MAX));
    let db = on(sim_backend::storage_with_capacity(
        &fs,
        Arc::clone(&capacity),
    ))
    .unwrap();
    for step in &WORKLOAD[..4] {
        apply(&db, *step).unwrap();
    }
    let used = sim_backend::length(&fs).unwrap();
    capacity.store(used, Ordering::Relaxed);
    let before = dump(&db);
    let mut refused = 0;
    for round in 0..200 {
        let parameters = Parameters::from([("r".to_owned(), Value::Integer(round))]);
        match db.execute(
            "UNWIND range(1, 50) AS i INSERT (:Fill {r: $r, i: i, text: 'word filler text'})",
            &parameters,
        ) {
            Ok(_) => {}
            Err(error) => {
                assert_eq!(error.kind(), ErrorKind::Storage, "{error}");
                refused += 1;
                if refused == 3 {
                    break;
                }
            }
        }
    }
    assert_eq!(refused, 3, "the device never filled up");
    let filled = db
        .execute("MATCH (n:Fill) RETURN count(*)", &Parameters::new())
        .unwrap()
        .rows()
        .to_vec();
    let on_full = dump(&db);
    assert_eq!(on_full[2..], before[2..]);
    assert_eq!(consistent(&db), Ok(()));
    capacity.store(u64::MAX, Ordering::Relaxed);
    assert_eq!(
        apply(&db, WORKLOAD[4]).map_err(|error| error.kind()),
        Err(ErrorKind::Storage),
        "redb refuses writes after an I/O error until the file is opened again"
    );
    assert_eq!(dump(&db)[2..], before[2..]);
    drop(db);
    capacity.store(used, Ordering::Relaxed);
    match on(sim_backend::storage_with_capacity(
        &fs,
        Arc::clone(&capacity),
    )) {
        Ok(db) => {
            assert_eq!(dump(&db)[2..], before[2..]);
            assert_eq!(consistent(&db), Ok(()));
        }
        Err(error) => assert_eq!(error.kind(), ErrorKind::Storage, "{error}"),
    }
    capacity.store(u64::MAX, Ordering::Relaxed);
    let db = on(sim_backend::storage_with_capacity(
        &fs,
        Arc::clone(&capacity),
    ))
    .unwrap();
    assert_eq!(
        db.execute("MATCH (n:Fill) RETURN count(*)", &Parameters::new())
            .unwrap()
            .rows(),
        filled
    );
    apply(&db, WORKLOAD[4]).unwrap();
    assert_eq!(consistent(&db), Ok(()));
    let live = dump(&db);
    drop(db);
    for mode in modes(2, 7) {
        let recovered = fs.crash(mode);
        let db = open(&recovered).unwrap();
        assert_eq!(consistent(&db), Ok(()), "{mode:?}");
        assert_eq!(dump(&db), live, "{mode:?}");
        assert_eq!(
            db.execute("MATCH (n:Fill) RETURN count(*)", &Parameters::new())
                .unwrap()
                .rows(),
            filled,
            "{mode:?}"
        );
    }
}
