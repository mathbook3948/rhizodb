use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use crate::database::Database;
use crate::engine::any_fs::AnyFs;
use crate::engine::sim_fs::Crash;
use crate::engine::sim_fs::Failure;
use crate::engine::sim_fs::SimFs;
use crate::engine::store::CheckpointMode;
use crate::engine::store::Engine;
use crate::engine::store::EngineOptions;
use crate::error::Error;
use crate::error::ErrorKind;
use crate::graph::IndexTarget;
use crate::query::Statements;
use crate::replication::Changes;
use crate::replication::Position;
use crate::result::Parameters;
use crate::result::Value;
use crate::test_scale;

const NAME: &str = "node.rhizodb";
const NODES: usize = 3;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound.max(1)
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

fn options() -> EngineOptions {
    EngineOptions {
        segment_target: 16 << 10,
        memtable_limit: 1 << 30,
        wal_limit: 1 << 40,
        checkpoint: CheckpointMode::Inline,
        cache_bytes: 256 * crate::engine::layout::BLOCK,
        reclaim_slack: 1 << 20,
        stage_floor: 8,
        ..EngineOptions::default()
    }
}

fn open(fs: &SimFs, replica: bool) -> Result<Database, Error> {
    let engine = Engine::open(Arc::new(AnyFs::Sim(fs.clone())), NAME, options())
        .map_err(crate::storage::StorageError::from)?;
    Ok(Database::from_parts(
        crate::database::graph_on(engine, replica)?,
        Statements::default(),
    ))
}

fn export(db: &Database) -> Vec<u8> {
    readable_export(db).expect("a database waiting for an upgrade has no export")
}

fn readable_export(db: &Database) -> Option<Vec<u8>> {
    let pending = db.graph.upgrade_pending();
    let mut out = Vec::new();
    match db.export(&mut out) {
        Ok(()) => {
            assert!(!pending, "a database waiting for an upgrade served a read");
            Some(out)
        }
        Err(error) if error.kind() == ErrorKind::FormatUpgradePending => {
            assert!(pending, "a ready database refused a read: {error}");
            None
        }
        Err(error) => panic!("an export failed with {error}"),
    }
}

const OLDER_FORMAT: u32 = 13;

fn stored_format(db: &Database) -> Option<Vec<u8>> {
    use crate::storage::ReadTransaction;
    use crate::storage::Storage;
    db.graph
        .storage()
        .begin_read()
        .unwrap()
        .get(crate::storage::Table::Metadata, b"format_version")
        .unwrap()
}

fn writer_id(node: usize) -> Vec<u8> {
    format!("node-{node}").into_bytes()
}

struct Node {
    fs: SimFs,
    db: Database,
    cut_off: bool,
    rebuilt: u32,
}

struct Delayed {
    to: usize,
    changes: Changes,
}

struct Sim {
    seed: u64,
    rolling: bool,
    rng: Rng,
    nodes: Vec<Node>,
    term: u64,
    holder: usize,
    stale: Option<usize>,
    seen: HashMap<Position, Vec<u8>>,
    in_flight: Vec<Delayed>,
    steps: Vec<String>,
    counts: std::collections::BTreeMap<&'static str, u32>,
}

impl Sim {
    fn new(seed: u64, rolling: bool) -> Self {
        let nodes = (0..NODES)
            .map(|index| {
                let fs = SimFs::new();
                let db = open(&fs, index != 0).unwrap();
                Node {
                    fs,
                    db,
                    cut_off: false,
                    rebuilt: 0,
                }
            })
            .collect();
        let mut sim = Self {
            seed,
            rolling,
            rng: Rng(seed),
            nodes,
            term: 1,
            holder: 0,
            stale: None,
            seen: HashMap::new(),
            in_flight: Vec::new(),
            steps: Vec::new(),
            counts: std::collections::BTreeMap::new(),
        };
        sim.seen.insert(
            Position::start(),
            export(&open(&SimFs::new(), true).unwrap()),
        );
        sim.note(0);
        sim.nodes[0].db.promote(1, &writer_id(0)).unwrap();
        sim.note(0);
        if rolling {
            use crate::storage::Storage;
            use crate::storage::WriteTransaction;
            let mut write = sim.nodes[0].db.graph.storage().begin_write().unwrap();
            write
                .put(
                    crate::storage::Table::Metadata,
                    b"format_version",
                    &OLDER_FORMAT.to_be_bytes(),
                )
                .unwrap();
            write.commit().unwrap();
            sim.note(0);
        }
        sim
    }

    fn context(&self) -> String {
        let tail = self.steps.len().saturating_sub(12);
        format!(
            "replication simulation seed {} failed after steps {:?}",
            self.seed,
            &self.steps[tail..]
        )
    }

    fn count(&mut self, event: &'static str) {
        *self.counts.entry(event).or_default() += 1;
    }

    fn note(&mut self, node: usize) {
        let db = &self.nodes[node].db;
        let position = db.position();
        let Some(exported) = readable_export(db) else {
            self.count("pending read");
            return;
        };
        if let Some(known) = self.seen.get(&position) {
            assert!(
                known == &exported,
                "{}: {position:?} holds two graphs",
                self.context()
            );
        }
        self.seen.insert(position, exported);
    }

    fn check_replica(&mut self, node: usize, before: &Position) {
        let db = &self.nodes[node].db;
        let position = db.position();
        assert!(
            position.term() >= before.term(),
            "{}: node {node} went from term {} to term {}",
            self.context(),
            before.term(),
            position.term()
        );
        let Some(exported) = readable_export(db) else {
            self.count("pending read");
            return;
        };
        let expected = self.seen.get(&position).unwrap_or_else(|| {
            panic!(
                "{}: node {node} reached {position:?}, which no writer passed",
                self.context()
            )
        });
        assert!(
            &exported == expected,
            "{}: node {node} at {position:?} holds another graph than its writer did",
            self.context()
        );
    }

    fn write(&mut self) {
        let node = if let Some(stale) = self.stale.filter(|_| self.rng.chance(40)) {
            stale
        } else {
            self.holder
        };
        let k = self.rng.below(1000) as i64;
        let parameters = Parameters::from([
            ("k".to_owned(), Value::Integer(k)),
            ("m".to_owned(), Value::Integer(k % 7)),
        ]);
        let query = match self.rng.below(10) {
            0..=4 => "INSERT (:N {k: $k, text: 'word ' + toString($k)})",
            5 | 6 => "MATCH (n:N) WHERE n.k % 7 = $m SET n.v = $k, n.text = 'graph'",
            7 => {
                "MATCH (a:N), (b:N) WHERE a.k % 7 = $m AND b.k = a.k + 1 INSERT (a)-[:R {w: $k}]->(b)"
            }
            8 => "MATCH (n:N) WHERE n.k % 11 = $m DETACH DELETE n",
            _ => "",
        };
        let db = &self.nodes[node].db;
        let result = if query.is_empty() {
            match self.rng.below(3) {
                0 => db
                    .create_property_index("by_k", IndexTarget::Label("N"), "k")
                    .map(drop),
                1 if !self.rolling => db
                    .create_keyword_index("text", IndexTarget::Label("N"), &["text"])
                    .map(drop),
                1 => db
                    .create_property_index("by_v", IndexTarget::Label("N"), "v")
                    .map(drop),
                _ => db.drop_index("text").map(drop),
            }
        } else {
            db.execute(query, &parameters).map(drop)
        };
        self.steps.push(format!("write on {node}"));
        match result {
            Ok(()) => {
                self.count(if Some(node) == self.stale {
                    "stale write"
                } else {
                    "write"
                });
                self.note(node);
            }
            Err(error) => assert!(
                matches!(
                    error.kind(),
                    ErrorKind::NotWriter | ErrorKind::IndexAlreadyExists
                ),
                "{}: a write failed with {error}",
                self.context()
            ),
        }
    }

    fn pull(&mut self) {
        let mut to = self.rng.below(NODES as u64) as usize;
        if (to == self.holder || Some(to) == self.stale) && self.rng.chance(85) {
            to = (to + 1 + self.rng.below(NODES as u64 - 1) as usize) % NODES;
        }
        let from = if self.rng.chance(60) {
            self.holder
        } else {
            self.rng.below(NODES as u64) as usize
        };
        if to == from || self.nodes[to].cut_off != self.nodes[from].cut_off {
            return;
        }
        let asked = self.nodes[to].db.position();
        let size = 256 << self.rng.below(6);
        self.steps.push(format!("pull {from} -> {to}"));
        let changes = match self.nodes[from].db.changes_since(&asked, size) {
            Ok(changes) => changes,
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::Diverged | ErrorKind::DifferentDatabase
                ) =>
            {
                self.count("diverged pull");
                self.resolve(to, from);
                return;
            }
            Err(error) => panic!("{}: changes_since failed with {error}", self.context()),
        };
        let changes = Changes::from_bytes(changes.as_bytes()).unwrap();
        if self.rng.chance(10) {
            self.count("lost");
            return;
        }
        if self.rng.chance(15) {
            self.count("delayed");
            self.in_flight.push(Delayed { to, changes });
            return;
        }
        self.deliver(to, &changes, from);
        if self.rng.chance(10) {
            self.count("duplicated");
            self.deliver(to, &changes, from);
        }
    }

    fn deliver_late(&mut self) {
        if self.in_flight.is_empty() {
            return;
        }
        let index = self.rng.below(self.in_flight.len() as u64) as usize;
        let late = self.in_flight.swap_remove(index);
        self.steps.push(format!("late delivery to {}", late.to));
        self.deliver(late.to, &late.changes, usize::MAX);
    }

    fn deliver(&mut self, to: usize, changes: &Changes, from: usize) {
        let before = self.nodes[to].db.position();
        let failing = self.rng.chance(8);
        if failing {
            self.nodes[to]
                .fs
                .fail_after(self.rng.below(40), Failure::FromThenOn);
        }
        let applied = self.nodes[to].db.apply(changes);
        self.nodes[to].fs.stop_failing();
        match applied {
            Ok(_) => {
                self.count("apply");
                self.check_replica(to, &before);
            }
            Err(error) => match error.kind() {
                ErrorKind::Storage | ErrorKind::CorruptData if failing => {
                    self.count("failed apply");
                    self.crash(to);
                }
                ErrorKind::Gap => self.count("gap"),
                ErrorKind::Fenced => self.count("fenced"),
                ErrorKind::NotReplica => self.count("not replica"),
                ErrorKind::Diverged | ErrorKind::DifferentDatabase if from != usize::MAX => {
                    self.count("diverged apply");
                    self.resolve(to, from);
                }
                ErrorKind::Diverged | ErrorKind::DifferentDatabase => {
                    self.count("diverged late apply")
                }
                _ => panic!("{}: apply failed with {error}", self.context()),
            },
        }
    }

    fn resolve(&mut self, a: usize, b: usize) {
        for node in [a, b] {
            if node == self.holder {
                continue;
            }
            let reaches = {
                let holder = &self.nodes[self.holder].db;
                holder
                    .changes_since(&self.nodes[node].db.position(), 1)
                    .is_ok()
            };
            if !reaches {
                self.rebuild(node);
            }
        }
    }

    fn rebuild(&mut self, node: usize) {
        self.steps.push(format!("rebuild {node}"));
        if self.stale == Some(node) {
            self.stale = None;
        }
        let fs = SimFs::new();
        self.nodes[node].db = open(&fs, true).unwrap();
        self.nodes[node].fs = fs;
        self.nodes[node].rebuilt += 1;
    }

    fn crash(&mut self, node: usize) {
        let mode = match self.rng.below(4) {
            0 => Crash::DropUnsynced,
            1 => Crash::KeepUnsynced,
            2 => Crash::Reorder {
                seed: self.rng.next(),
            },
            _ => Crash::Scramble {
                seed: self.rng.next(),
            },
        };
        self.steps.push(format!("crash {node} {mode:?}"));
        self.count("crash");
        let before = self.nodes[node].db.position();
        let recovered = self.nodes[node].fs.crash(mode);
        let writer = node == self.holder || self.stale == Some(node);
        let db = open(&recovered, false)
            .unwrap_or_else(|error| panic!("{}: reopening failed with {error}", self.context()));
        self.nodes[node].db = db;
        self.nodes[node].fs = recovered;
        let after = self.nodes[node].db.position();
        assert!(
            after.term() >= before.term() || writer,
            "{}: a crash took node {node} back from term {}",
            self.context(),
            before.term()
        );
        if node == self.holder {
            let term = before.term();
            self.nodes[node].db.promote(term, &writer_id(node)).unwrap();
            self.note(node);
        } else if writer {
            assert_ne!(self.nodes[node].db.role(), crate::replication::Role::Writer);
        } else {
            self.check_replica(node, &Position::start());
        }
    }

    fn sync(&mut self, to: usize, from: usize) -> bool {
        for _ in 0..10_000 {
            let asked = self.nodes[to].db.position();
            let changes = match self.nodes[from].db.changes_since(&asked, 1 << 16) {
                Ok(changes) => changes,
                Err(_) => return false,
            };
            if changes.is_empty() {
                return true;
            }
            if self.nodes[to].db.apply(&changes).is_err() {
                return false;
            }
            self.check_replica(to, &asked);
        }
        false
    }

    fn planned_handover(&mut self) {
        let next = (self.holder + 1 + self.rng.below(NODES as u64 - 1) as usize) % NODES;
        if self.nodes[next].cut_off != self.nodes[self.holder].cut_off || self.stale.is_some() {
            return;
        }
        self.steps
            .push(format!("handover {} -> {next}", self.holder));
        self.count("handover");
        let last = self.nodes[self.holder]
            .db
            .demote(std::time::Duration::from_secs(5))
            .unwrap();
        self.note(self.holder);
        if !self.sync(next, self.holder) {
            self.rebuild(next);
            assert!(
                self.sync(next, self.holder),
                "{}: a rebuilt node cannot catch up",
                self.context()
            );
        }
        assert_eq!(self.nodes[next].db.position(), last, "{}", self.context());
        self.term += 1;
        self.promote(next).unwrap();
        self.holder = next;
    }

    fn promote(&mut self, node: usize) -> Result<(), Error> {
        let before = self.nodes[node].db.position();
        let held = readable_export(&self.nodes[node].db);
        self.nodes[node].db.promote(self.term, &writer_id(node))?;
        let begun = self.nodes[node].db.changes_since(&before, 1).unwrap();
        if let Some(held) = held {
            self.seen.insert(begun.last().clone(), held);
        }
        self.note(node);
        Ok(())
    }

    fn failover(&mut self) {
        if self.stale.is_some() {
            return;
        }
        let next = (self.holder + 1 + self.rng.below(NODES as u64 - 1) as usize) % NODES;
        self.steps
            .push(format!("failover {} -> {next}", self.holder));
        self.count("failover");
        self.term += 1;
        if self.promote(next).is_err() {
            self.rebuild(next);
            self.promote(next).unwrap();
        }
        self.stale = Some(self.holder);
        self.holder = next;
    }

    fn fence_stale(&mut self) {
        let Some(stale) = self.stale.take() else {
            return;
        };
        if self.rng.chance(50) {
            self.steps.push(format!("tell stale {stale} the new term"));
            self.count("step aside");
            let newest = self.nodes[self.holder].db.position();
            drop(self.nodes[stale].db.changes_since(&newest, 1));
        }
        self.steps.push(format!("demote stale {stale}"));
        self.nodes[stale]
            .db
            .demote(std::time::Duration::from_secs(5))
            .unwrap();
        self.note(stale);
    }

    fn partition(&mut self) {
        let node = self.rng.below(NODES as u64) as usize;
        self.nodes[node].cut_off = !self.nodes[node].cut_off;
        self.steps
            .push(format!("partition {node} {}", self.nodes[node].cut_off));
    }

    fn heal_and_converge(&mut self) {
        self.steps.push("heal".to_owned());
        self.fence_stale();
        if stored_format(&self.nodes[self.holder].db) == Some(OLDER_FORMAT.to_be_bytes().to_vec()) {
            self.steps
                .push(format!("restart {} on the new build", self.holder));
            self.count("upgrade at heal");
            let holder = self.holder;
            let fs = self.nodes[holder].fs.clone();
            let placeholder = open(&SimFs::new(), true).unwrap();
            drop(std::mem::replace(&mut self.nodes[holder].db, placeholder));
            self.nodes[holder].db = open(&fs, false).unwrap();
            let term = self.nodes[holder].db.position().term();
            self.nodes[holder]
                .db
                .promote(term, &writer_id(holder))
                .unwrap();
            self.note(holder);
        }
        for node in &mut self.nodes {
            node.cut_off = false;
        }
        self.in_flight.clear();
        for node in 0..NODES {
            if node == self.holder {
                continue;
            }
            if !self.sync(node, self.holder) {
                self.rebuild(node);
                assert!(
                    self.sync(node, self.holder),
                    "{}: no convergence",
                    self.context()
                );
            }
        }
        let position = self.nodes[self.holder].db.position();
        let exported = export(&self.nodes[self.holder].db);
        for node in 0..NODES {
            assert_eq!(
                self.nodes[node].db.position(),
                position,
                "{}",
                self.context()
            );
            assert!(
                export(&self.nodes[node].db) == exported,
                "{}",
                self.context()
            );
            self.nodes[node].db.check().unwrap();
            let violations = self.nodes[node]
                .db
                .graph
                .read()
                .and_then(|read| read.check_integrity())
                .unwrap();
            assert!(violations.is_empty(), "{}: {violations:?}", self.context());
        }
    }

    fn check_pairs(&self) {
        let held: Vec<(Position, Option<Vec<u8>>)> = self
            .nodes
            .iter()
            .map(|node| (node.db.position(), readable_export(&node.db)))
            .collect();
        for (left, (a, a_export)) in held.iter().enumerate() {
            for (b, b_export) in &held[left + 1..] {
                if (a.term(), a.writer_id(), a.lsn()) == (b.term(), b.writer_id(), b.lsn()) {
                    assert_eq!(a.chain(), b.chain(), "{}", self.context());
                    if let (Some(a_export), Some(b_export)) = (a_export, b_export) {
                        assert!(
                            a_export == b_export,
                            "{}: {a:?} holds two graphs",
                            self.context()
                        );
                    }
                }
            }
        }
    }

    fn run(&mut self, steps: usize) {
        for _ in 0..steps {
            self.check_pairs();
            match self.rng.below(100) {
                0..=34 => self.write(),
                35..=69 => self.pull(),
                70..=76 => self.deliver_late(),
                77..=80 => {
                    let node = self.rng.below(NODES as u64) as usize;
                    self.crash(node);
                }
                81..=84 => self.planned_handover(),
                85..=87 => self.failover(),
                88..=91 => self.fence_stale(),
                _ => self.partition(),
            }
        }
        self.heal_and_converge();
    }
}

#[test]
#[ignore = "left out of mise run check, run by mise run random"]
fn replicas_converge_on_their_writer_under_random_faults() {
    let seeds = test_scale::cases(6);
    let first = std::env::var("RHIZODB_REPLICATION_SEED")
        .ok()
        .and_then(|seed| seed.parse::<u64>().ok());
    let seeds: Vec<u64> = match first {
        Some(seed) => vec![seed],
        None => (0..u64::from(seeds))
            .map(|seed| 0x5EED_0000 + seed)
            .collect(),
    };
    for seed in seeds {
        eprintln!("replication simulation seed {seed}");
        let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let mut sim = Sim::new(seed, false);
            sim.run(300);
            let rebuilt = sim.nodes.iter().map(|node| node.rebuilt).sum::<u32>();
            sim.counts.insert("rebuild", rebuilt);
            sim.counts
        }));
        match outcome {
            Ok(counts) => eprintln!("seed {seed} converged: {counts:?}"),
            Err(panic) => {
                eprintln!("rerun with RHIZODB_REPLICATION_SEED={seed}");
                std::panic::resume_unwind(panic);
            }
        }
    }
}

#[test]
#[ignore = "left out of mise run check, run by mise run random"]
fn replicas_converge_through_a_rolling_upgrade_under_random_faults() {
    let first = std::env::var("RHIZODB_REPLICATION_SEED")
        .ok()
        .and_then(|seed| seed.parse::<u64>().ok());
    let seeds: Vec<u64> = match first {
        Some(seed) => vec![seed],
        None => (0..u64::from(test_scale::cases(6)))
            .map(|seed| 0x5EED_1000 + seed)
            .collect(),
    };
    for seed in seeds {
        eprintln!("rolling upgrade simulation seed {seed}");
        let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let mut sim = Sim::new(seed, true);
            sim.run(300);
            for node in &sim.nodes {
                assert_ne!(
                    stored_format(&node.db),
                    Some(OLDER_FORMAT.to_be_bytes().to_vec()),
                    "{}",
                    sim.context()
                );
            }
            sim.counts
        }));
        match outcome {
            Ok(counts) => eprintln!("seed {seed} converged: {counts:?}"),
            Err(panic) => {
                eprintln!("rerun with RHIZODB_REPLICATION_SEED={seed}");
                std::panic::resume_unwind(panic);
            }
        }
    }
}
