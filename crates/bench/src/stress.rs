//! Load scenarios: many readers on one shared database, readers beside a writer that commits
//! without pause, and a writer alone for a long run. Every read is checked against the answer
//! computed from the dataset, and every write is checked once the run ends, so a wrong answer
//! under concurrency fails the run instead of only slowing it.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::Barrier;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use serde_json::Value;
use serde_json::json;

use crate::data::Dataset;
use crate::engine::Engine;
use crate::engine::EngineError;
use crate::engine::EngineKind;
use crate::engine::Instance;
use crate::engine::Query;
use crate::engine::Session;
use crate::engine::Texts;
use crate::fingerprint::Fingerprint;
use crate::fingerprint::RowHasher;
use crate::random::Random;
use crate::runner::Environment;

const POINT_LOOKUP: Texts = Texts::graph("MATCH (p:Person {id: $id}) RETURN p.name");
const FILTER_SCAN: Texts = Texts::graph("MATCH (p:Person) WHERE p.age > 30 RETURN count(*)");
const TWO_HOP: Texts = Texts::graph(
    "MATCH (a:Person {id: $id})-[:KNOWS]->(:Person)-[:KNOWS]->(c:Person) RETURN count(DISTINCT c)",
);
const CREATE: Texts = Texts::graph(
    "MATCH (b:Person {id: $to}) CREATE (a:Person {id: $id, name: $name})-[:KNOWS {since: $since}]->(b)",
);
const RENAME: Texts = Texts::graph("MATCH (p:Person {id: $id}) SET p.name = $name");
const WRITTEN_NODES: Texts =
    Texts::graph("MATCH (p:Person) WHERE p.id > $base RETURN p.id, p.name");
const WRITTEN_EDGES: Texts =
    Texts::graph("MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE a.id > $base RETURN a.id, b.id");

const SAMPLE_EVERY: Duration = Duration::from_millis(100);
const WARM_UP: Duration = Duration::from_millis(500);
const SCAN_SHARE: usize = 10;
const HOP_SHARE: usize = 30;
const RENAME_EVERY: u64 = 4;
const STALL: Duration = Duration::from_millis(100);

/// A load scenario.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Scenario {
    /// Reader threads alone on one shared database.
    Readers,
    /// The same reader threads while one more thread commits small writes without pause.
    Mixed,
    /// One thread committing small writes for the whole run, sampled every second.
    Sustained,
}

impl Scenario {
    /// Every scenario, in the order a run measures them.
    pub const ALL: [Scenario; 3] = [Scenario::Readers, Scenario::Mixed, Scenario::Sustained];

    /// The name `--load` takes.
    pub fn name(self) -> &'static str {
        match self {
            Scenario::Readers => "readers",
            Scenario::Mixed => "mixed",
            Scenario::Sustained => "sustained",
        }
    }

    /// Reads a comma-separated list of names, or `all`.
    pub fn parse_list(text: &str) -> Option<Vec<Scenario>> {
        if text == "all" {
            return Some(Scenario::ALL.to_vec());
        }
        text.split(',')
            .map(|name| Scenario::ALL.into_iter().find(|s| s.name() == name))
            .collect()
    }

    fn reads(self) -> bool {
        self != Scenario::Sustained
    }

    fn writes(self) -> bool {
        self != Scenario::Readers
    }
}

/// How a load run is shaped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Load {
    /// The scenarios to run. None runs the workload suite instead.
    pub scenarios: Vec<Scenario>,
    /// The reader counts each reading scenario runs with.
    pub threads: Vec<usize>,
    /// How long each measured run lasts.
    pub duration: Duration,
}

impl Default for Load {
    fn default() -> Self {
        Load {
            scenarios: Vec::new(),
            threads: vec![1, 2, 4, 8],
            duration: Duration::from_secs(10),
        }
    }
}

/// One point of a scenario: the scenario and how many readers it ran with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Point {
    pub scenario: Scenario,
    pub readers: usize,
}

impl Point {
    /// Every point `load` asks for.
    pub fn all(load: &Load) -> Vec<Point> {
        let mut points = Vec::new();
        for &scenario in &load.scenarios {
            if scenario.reads() {
                for &readers in &load.threads {
                    points.push(Point { scenario, readers });
                }
            } else {
                points.push(Point {
                    scenario,
                    readers: 0,
                });
            }
        }
        points
    }

    /// The name the report prints.
    pub fn label(self) -> String {
        match self.scenario {
            Scenario::Sustained => self.scenario.name().to_owned(),
            _ => format!("{}/{}-threads", self.scenario.name(), self.readers),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Read {
    Lookup(i64),
    Scan,
    TwoHop(i64),
}

impl Read {
    fn kind(self) -> &'static str {
        match self {
            Read::Lookup(_) => "point-lookup",
            Read::Scan => "filter-scan",
            Read::TwoHop(_) => "2-hop",
        }
    }
}

/// The exact answer of every read the mix can draw, computed from the dataset.
#[derive(Debug)]
pub struct Answers {
    names: HashMap<i64, String>,
    scan: i64,
    two_hop: HashMap<i64, i64>,
    probes: Vec<i64>,
    persons: i64,
    first_written: i64,
}

impl Answers {
    /// Computes the answers of every probe of `data`.
    pub fn of(data: &Dataset) -> Answers {
        let mut out: HashMap<i64, Vec<i64>> = HashMap::new();
        for edge in &data.knows {
            out.entry(edge.from).or_default().push(edge.to);
        }
        let none = Vec::new();
        let two_hop = data
            .probes
            .iter()
            .map(|&probe| {
                let reached: BTreeSet<i64> = out
                    .get(&probe)
                    .unwrap_or(&none)
                    .iter()
                    .flat_map(|middle| out.get(middle).unwrap_or(&none).iter().copied())
                    .collect();
                (probe, reached.len() as i64)
            })
            .collect();
        let scan = data
            .people
            .iter()
            .filter(|person| person.age.is_some_and(|age| age > 30))
            .count() as i64;
        Answers {
            names: data
                .people
                .iter()
                .map(|person| (person.id, person.name.clone()))
                .collect(),
            scan,
            two_hop,
            probes: data.probes.clone(),
            persons: data.people.len() as i64,
            first_written: data.people.len() as i64 + 1,
        }
    }

    fn draw(&self, random: &mut Random) -> Read {
        let probe = self.probes.get(random.below(self.probes.len())).copied();
        let roll = random.below(100);
        match probe {
            Some(id) if roll < HOP_SHARE => Read::TwoHop(id),
            _ if roll < HOP_SHARE + SCAN_SHARE => Read::Scan,
            Some(id) => Read::Lookup(id),
            None => Read::Scan,
        }
    }

    fn expected(&self, read: Read) -> Fingerprint {
        let mut row = RowHasher::default();
        match read {
            Read::Lookup(id) => match self.names.get(&id) {
                Some(name) => row.text(name),
                None => return Fingerprint::default(),
            },
            Read::Scan => row.int(self.scan),
            Read::TwoHop(id) => row.int(self.two_hop.get(&id).copied().unwrap_or(0)),
        }
        let mut fingerprint = Fingerprint::default();
        fingerprint.absorb(row);
        fingerprint
    }
}

/// The latency distribution of one kind of operation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Latency {
    pub count: u64,
    pub p50: Duration,
    pub p99: Duration,
    pub max: Duration,
}

impl Latency {
    /// The distribution of `samples`, which it sorts.
    pub fn of(samples: &mut [Duration]) -> Latency {
        samples.sort_unstable();
        let at = |fraction: f64| -> Duration {
            let index = ((samples.len() as f64 * fraction).ceil() as usize).saturating_sub(1);
            samples.get(index).copied().unwrap_or_default()
        };
        Latency {
            count: samples.len() as u64,
            p50: at(0.5),
            p99: at(0.99),
            max: samples.last().copied().unwrap_or_default(),
        }
    }

    fn json(self) -> Value {
        json!({
            "count": self.count,
            "p50_us": self.p50.as_micros() as u64,
            "p99_us": self.p99.as_micros() as u64,
            "max_us": self.max.as_micros() as u64,
        })
    }
}

#[derive(Clone, Copy, Debug)]
struct Op {
    end: Duration,
    took: Duration,
}

#[derive(Debug, Default)]
struct ReaderLog {
    ops: Vec<Op>,
    by_kind: BTreeMap<&'static str, Vec<Duration>>,
    errors: u64,
    wrong: u64,
    failure: Option<String>,
}

#[derive(Debug, Default)]
struct WriterLog {
    ops: Vec<Op>,
    names: BTreeMap<i64, String>,
    edges: Vec<(i64, i64)>,
    errors: u64,
    failure: Option<String>,
}

/// The size of the database files and the memory of the process at one instant of a run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sample {
    pub at: Duration,
    pub wal_bytes: u64,
    pub data_bytes: u64,
    pub rss_bytes: Option<u64>,
}

/// One second of a run.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Second {
    pub second: u64,
    pub reads: Latency,
    pub writes: Latency,
    pub wal_bytes: u64,
    pub data_bytes: u64,
    pub rss_bytes: Option<u64>,
    /// Whether the log shrank in this second, which marks a checkpoint that finished.
    pub checkpoint: bool,
}

/// What one run of one point did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LoadRun {
    pub elapsed: Duration,
    pub reads: Latency,
    pub reads_by_kind: BTreeMap<&'static str, Latency>,
    pub writes: Latency,
    /// The longest time no reader finished a read.
    pub longest_read_gap: Duration,
    /// The reads that took longer than 100 ms.
    pub stalled_reads: u64,
    /// The p99 of the reads that ended in a second whose log shrank, against the others.
    pub read_p99_checkpointing: Option<Duration>,
    pub read_p99_otherwise: Option<Duration>,
    pub checkpoints: u64,
    pub errors: u64,
    /// The reads that returned another answer than the dataset gives, and the writes the final
    /// check found missing or changed.
    pub wrong: u64,
    pub failure: Option<String>,
    pub rss_growth_bytes: Option<i64>,
    pub timeline: Vec<Second>,
}

impl LoadRun {
    /// The reads per second.
    pub fn read_throughput(&self) -> f64 {
        per_second(self.reads.count, self.elapsed)
    }

    /// The commits per second.
    pub fn write_throughput(&self) -> f64 {
        per_second(self.writes.count, self.elapsed)
    }

    /// Whether every operation succeeded with the right answer.
    pub fn clean(&self) -> bool {
        self.errors == 0 && self.wrong == 0 && self.failure.is_none()
    }
}

fn per_second(count: u64, elapsed: Duration) -> f64 {
    if elapsed.is_zero() {
        0.0
    } else {
        count as f64 / elapsed.as_secs_f64()
    }
}

fn text_for(engine: &dyn Engine, texts: &Texts) -> Result<&'static str, EngineError> {
    texts.for_engine(engine.kind(), engine.dialect())
}

struct Texts3 {
    lookup: &'static str,
    scan: &'static str,
    hop: &'static str,
}

impl Texts3 {
    fn new(engine: &dyn Engine) -> Result<Texts3, EngineError> {
        Ok(Texts3 {
            lookup: text_for(engine, &POINT_LOOKUP)?,
            scan: text_for(engine, &FILTER_SCAN)?,
            hop: text_for(engine, &TWO_HOP)?,
        })
    }

    fn query(&self, read: Read) -> Query<'static> {
        match read {
            Read::Lookup(id) => Query::new(self.lookup).int("id", id),
            Read::Scan => Query::new(self.scan),
            Read::TwoHop(id) => Query::new(self.hop).int("id", id),
        }
    }

    fn prepare(&self, session: &mut dyn Session) -> Result<(), EngineError> {
        session.prepare(self.lookup)?;
        session.prepare(self.scan)?;
        session.prepare(self.hop)
    }
}

/// Runs `point` once on the database in `dir`, which holds the loaded graph, and closes it.
pub fn run_point(
    engine: &dyn Engine,
    dir: &Path,
    point: Point,
    answers: &Answers,
    duration: Duration,
    seed: u64,
) -> Result<LoadRun, EngineError> {
    let texts = Texts3::new(engine)?;
    let create = text_for(engine, &CREATE)?;
    let rename = text_for(engine, &RENAME)?;
    let instance = engine.open(dir)?;
    warm_up(&*instance, &texts, answers, seed)?;
    let readers = if point.scenario.reads() {
        point.readers
    } else {
        0
    };
    let writing = point.scenario.writes();
    let stop = AtomicBool::new(false);
    let barrier = Barrier::new(readers + usize::from(writing) + 2);
    let start_rss = rss_bytes();
    let (elapsed, reader_logs, writer_log, samples) = thread::scope(|scope| {
        let instance = &*instance;
        let stop = &stop;
        let barrier = &barrier;
        let texts = &texts;
        let reader_handles: Vec<_> = (0..readers)
            .map(|reader| {
                scope.spawn(move || {
                    read_loop(
                        instance,
                        texts,
                        answers,
                        seed ^ (reader as u64 + 1),
                        barrier,
                        stop,
                    )
                })
            })
            .collect();
        let writer_handle = writing.then(|| {
            scope.spawn(move || write_loop(instance, create, rename, answers, barrier, stop))
        });
        let monitor = scope.spawn(move || monitor(dir, barrier, stop));
        barrier.wait();
        let start = Instant::now();
        thread::sleep(duration);
        stop.store(true, Ordering::Release);
        let reader_logs: Vec<ReaderLog> = reader_handles
            .into_iter()
            .map(|handle| handle.join().unwrap_or_else(|_| crashed_reader()))
            .collect();
        let writer_log = writer_handle.map(|handle| {
            handle.join().unwrap_or_else(|_| WriterLog {
                errors: 1,
                failure: Some("the writer thread panicked".to_owned()),
                ..WriterLog::default()
            })
        });
        let elapsed = start.elapsed();
        let samples = monitor.join().unwrap_or_default();
        (elapsed, reader_logs, writer_log, samples)
    });
    let mut run = summarize(
        elapsed,
        reader_logs,
        writer_log.as_ref(),
        &samples,
        start_rss,
    );
    if let Some(log) = &writer_log {
        let wrong = check_writes(&*instance, engine, answers, log)?;
        run.wrong += wrong;
        instance.close()?;
        if point.scenario == Scenario::Sustained {
            let reopened = engine.open(dir)?;
            let lost = check_writes(&*reopened, engine, answers, log)?;
            reopened.close()?;
            if lost > 0 {
                run.wrong += lost;
                run.failure
                    .get_or_insert_with(|| format!("{lost} writes differ after a reopen"));
            }
        }
    } else {
        instance.close()?;
    }
    Ok(run)
}

fn crashed_reader() -> ReaderLog {
    ReaderLog {
        errors: 1,
        failure: Some("a reader thread panicked".to_owned()),
        ..ReaderLog::default()
    }
}

fn warm_up(
    instance: &dyn Instance,
    texts: &Texts3,
    answers: &Answers,
    seed: u64,
) -> Result<(), EngineError> {
    let mut session = instance.session()?;
    texts.prepare(&mut *session)?;
    let mut random = Random::new(seed);
    let until = Instant::now() + WARM_UP;
    while Instant::now() < until {
        let read = answers.draw(&mut random);
        session.run(&texts.query(read))?;
    }
    Ok(())
}

fn read_loop(
    instance: &dyn Instance,
    texts: &Texts3,
    answers: &Answers,
    seed: u64,
    barrier: &Barrier,
    stop: &AtomicBool,
) -> ReaderLog {
    let mut log = ReaderLog::default();
    let session = instance.session().and_then(|mut session| {
        texts.prepare(&mut *session)?;
        Ok(session)
    });
    barrier.wait();
    let mut session = match session {
        Ok(session) => session,
        Err(error) => {
            log.errors += 1;
            log.failure = Some(error.to_string());
            return log;
        }
    };
    let mut random = Random::new(seed);
    let start = Instant::now();
    while !stop.load(Ordering::Acquire) {
        let read = answers.draw(&mut random);
        let began = Instant::now();
        let result = session.run(&texts.query(read));
        let took = began.elapsed();
        match result {
            Ok(found) => {
                if found != answers.expected(read) {
                    log.wrong += 1;
                    log.failure.get_or_insert_with(|| {
                        format!(
                            "{read:?} returned {found:?}, the dataset gives {:?}",
                            answers.expected(read)
                        )
                    });
                }
                log.ops.push(Op {
                    end: start.elapsed(),
                    took,
                });
                log.by_kind.entry(read.kind()).or_default().push(took);
            }
            Err(error) => {
                log.errors += 1;
                log.failure.get_or_insert_with(|| error.to_string());
            }
        }
    }
    log
}

fn write_loop(
    instance: &dyn Instance,
    create: &'static str,
    rename: &'static str,
    answers: &Answers,
    barrier: &Barrier,
    stop: &AtomicBool,
) -> WriterLog {
    let mut log = WriterLog::default();
    let session = instance.session().and_then(|mut session| {
        session.prepare(create)?;
        session.prepare(rename)?;
        Ok(session)
    });
    barrier.wait();
    let mut session = match session {
        Ok(session) => session,
        Err(error) => {
            log.errors += 1;
            log.failure = Some(error.to_string());
            return log;
        }
    };
    let mut random = Random::new(answers.first_written as u64);
    let mut next = answers.first_written;
    let mut commits = 0_u64;
    let start = Instant::now();
    while !stop.load(Ordering::Acquire) {
        commits += 1;
        let renaming = commits.is_multiple_of(RENAME_EVERY) && next > answers.first_written;
        let (query, change) = if renaming {
            let written = usize::try_from(next - answers.first_written).unwrap_or(0);
            let id = answers.first_written + random.below(written) as i64;
            let name = format!("renamed {commits}");
            (
                Query::new(rename).int("id", id).text("name", &name),
                (id, name, None),
            )
        } else {
            let id = next;
            next += 1;
            let to = random.below(usize::try_from(answers.persons).unwrap_or(0)) as i64 + 1;
            let name = format!("written {id}");
            (
                Query::new(create)
                    .int("id", id)
                    .text("name", &name)
                    .int("to", to)
                    .int("since", 2025),
                (id, name, Some(to)),
            )
        };
        let began = Instant::now();
        let result = session.run(&query);
        let took = began.elapsed();
        match result {
            Ok(_) => {
                log.ops.push(Op {
                    end: start.elapsed(),
                    took,
                });
                let (id, name, edge) = change;
                log.names.insert(id, name);
                if let Some(to) = edge {
                    log.edges.push((id, to));
                }
            }
            Err(error) => {
                log.errors += 1;
                log.failure.get_or_insert_with(|| error.to_string());
                if !renaming {
                    next -= 1;
                }
            }
        }
    }
    log
}

fn check_writes(
    instance: &dyn Instance,
    engine: &dyn Engine,
    answers: &Answers,
    log: &WriterLog,
) -> Result<u64, EngineError> {
    let mut session = instance.session()?;
    let base = answers.first_written - 1;
    let nodes = session.run(&Query::new(text_for(engine, &WRITTEN_NODES)?).int("base", base))?;
    let edges = session.run(&Query::new(text_for(engine, &WRITTEN_EDGES)?).int("base", base))?;
    let mut expected_nodes = Fingerprint::default();
    for (id, name) in &log.names {
        let mut row = RowHasher::default();
        row.int(*id);
        row.text(name);
        expected_nodes.absorb(row);
    }
    let mut expected_edges = Fingerprint::default();
    for (from, to) in &log.edges {
        let mut row = RowHasher::default();
        row.int(*from);
        row.int(*to);
        expected_edges.absorb(row);
    }
    let differ = |found: Fingerprint, expected: Fingerprint| -> u64 {
        if found == expected {
            0
        } else {
            found.rows.abs_diff(expected.rows).max(1)
        }
    };
    Ok(differ(nodes, expected_nodes) + differ(edges, expected_edges))
}

fn monitor(dir: &Path, barrier: &Barrier, stop: &AtomicBool) -> Vec<Sample> {
    barrier.wait();
    let start = Instant::now();
    let mut samples = Vec::new();
    loop {
        let (wal_bytes, data_bytes) = file_bytes(dir);
        samples.push(Sample {
            at: start.elapsed(),
            wal_bytes,
            data_bytes,
            rss_bytes: rss_bytes(),
        });
        if stop.load(Ordering::Acquire) {
            return samples;
        }
        thread::sleep(SAMPLE_EVERY);
    }
}

/// The bytes of the log files and of every other file in `dir`. A file whose name holds `wal`
/// counts as log.
pub fn file_bytes(dir: &Path) -> (u64, u64) {
    let mut wal = 0;
    let mut data = 0;
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let size = entry.metadata().map_or(0, |metadata| metadata.len());
            if entry.file_name().to_string_lossy().contains("wal") {
                wal += size;
            } else {
                data += size;
            }
        }
    }
    (wal, data)
}

/// The resident memory of this process, where the system reports it.
pub fn rss_bytes() -> Option<u64> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|line| line.starts_with("VmRSS:"))?;
    let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kib * 1024)
}

fn summarize(
    elapsed: Duration,
    readers: Vec<ReaderLog>,
    writer: Option<&WriterLog>,
    samples: &[Sample],
    start_rss: Option<u64>,
) -> LoadRun {
    let mut run = LoadRun {
        elapsed,
        ..LoadRun::default()
    };
    let mut reads: Vec<Op> = Vec::new();
    let mut by_kind: BTreeMap<&'static str, Vec<Duration>> = BTreeMap::new();
    for log in readers {
        run.errors += log.errors;
        run.wrong += log.wrong;
        if run.failure.is_none() {
            run.failure = log.failure;
        }
        reads.extend(log.ops);
        for (kind, mut samples) in log.by_kind {
            by_kind.entry(kind).or_default().append(&mut samples);
        }
    }
    let writes: Vec<Op> = writer.map(|log| log.ops.clone()).unwrap_or_default();
    if let Some(log) = writer {
        run.errors += log.errors;
        if run.failure.is_none() {
            run.failure = log.failure.clone();
        }
    }
    run.reads = Latency::of(&mut reads.iter().map(|op| op.took).collect::<Vec<_>>());
    run.writes = Latency::of(&mut writes.iter().map(|op| op.took).collect::<Vec<_>>());
    run.reads_by_kind = by_kind
        .into_iter()
        .map(|(kind, mut samples)| (kind, Latency::of(&mut samples)))
        .collect();
    run.stalled_reads = reads.iter().filter(|op| op.took > STALL).count() as u64;
    let mut ends: Vec<Duration> = reads.iter().map(|op| op.end).collect();
    ends.sort_unstable();
    run.longest_read_gap = ends
        .windows(2)
        .filter_map(|pair| match pair {
            [earlier, later] => Some(later.saturating_sub(*earlier)),
            _ => None,
        })
        .max()
        .unwrap_or_default();
    run.timeline = timeline(elapsed, &reads, &writes, samples);
    run.checkpoints = run
        .timeline
        .iter()
        .filter(|second| second.checkpoint)
        .count() as u64;
    let checkpointing: BTreeSet<u64> = run
        .timeline
        .iter()
        .filter(|second| second.checkpoint)
        .map(|second| second.second)
        .collect();
    let (mut inside, mut outside): (Vec<Duration>, Vec<Duration>) = (Vec::new(), Vec::new());
    for op in &reads {
        if checkpointing.contains(&op.end.as_secs()) {
            inside.push(op.took);
        } else {
            outside.push(op.took);
        }
    }
    if !reads.is_empty() {
        run.read_p99_checkpointing = (!inside.is_empty()).then(|| Latency::of(&mut inside).p99);
        run.read_p99_otherwise = (!outside.is_empty()).then(|| Latency::of(&mut outside).p99);
    }
    let end_rss = samples.last().and_then(|sample| sample.rss_bytes);
    run.rss_growth_bytes = match (start_rss, end_rss) {
        (Some(start), Some(end)) => Some(end as i64 - start as i64),
        _ => None,
    };
    run
}

fn timeline(elapsed: Duration, reads: &[Op], writes: &[Op], samples: &[Sample]) -> Vec<Second> {
    let seconds = elapsed.as_secs() + 1;
    let bucket = |ops: &[Op], second: u64| -> Latency {
        let mut took: Vec<Duration> = ops
            .iter()
            .filter(|op| op.end.as_secs() == second)
            .map(|op| op.took)
            .collect();
        Latency::of(&mut took)
    };
    let mut previous_wal: Option<u64> = None;
    (0..seconds)
        .map(|second| {
            let within: Vec<&Sample> = samples
                .iter()
                .filter(|sample| sample.at.as_secs() == second)
                .collect();
            let mut checkpoint = false;
            for sample in &within {
                if previous_wal.is_some_and(|wal| sample.wal_bytes < wal) {
                    checkpoint = true;
                }
                previous_wal = Some(sample.wal_bytes);
            }
            let last = within.last().copied().copied().unwrap_or_default();
            Second {
                second,
                reads: bucket(reads, second),
                writes: bucket(writes, second),
                wal_bytes: last.wal_bytes,
                data_bytes: last.data_bytes,
                rss_bytes: last.rss_bytes,
                checkpoint,
            }
        })
        .collect()
}

impl LoadRun {
    /// The run as JSON, with its timeline.
    pub fn json(&self) -> Value {
        json!({
            "elapsed_s": self.elapsed.as_secs_f64(),
            "read_throughput": self.read_throughput(),
            "write_throughput": self.write_throughput(),
            "reads": self.reads.json(),
            "reads_by_kind": self.reads_by_kind.iter().map(|(kind, latency)| (kind.to_string(), latency.json())).collect::<serde_json::Map<_, _>>(),
            "writes": self.writes.json(),
            "longest_read_gap_us": self.longest_read_gap.as_micros() as u64,
            "stalled_reads": self.stalled_reads,
            "read_p99_checkpointing_us": self.read_p99_checkpointing.map(|p99| p99.as_micros() as u64),
            "read_p99_otherwise_us": self.read_p99_otherwise.map(|p99| p99.as_micros() as u64),
            "checkpoints": self.checkpoints,
            "errors": self.errors,
            "wrong": self.wrong,
            "failure": self.failure,
            "rss_growth_bytes": self.rss_growth_bytes,
            "timeline": self.timeline.iter().map(|second| json!({
                "second": second.second,
                "reads": second.reads.json(),
                "writes": second.writes.json(),
                "wal_bytes": second.wal_bytes,
                "data_bytes": second.data_bytes,
                "rss_bytes": second.rss_bytes,
                "checkpoint": second.checkpoint,
            })).collect::<Vec<_>>(),
        })
    }
}

/// Every run of one point on one engine, or why the engine could not run it.
#[derive(Clone, Debug)]
pub struct EngineRuns {
    pub engine: EngineKind,
    pub runs: Vec<LoadRun>,
    pub failure: Option<String>,
}

impl EngineRuns {
    /// The median of `value` over the runs.
    pub fn median(&self, value: impl Fn(&LoadRun) -> f64) -> Option<f64> {
        let mut values: Vec<f64> = self.runs.iter().map(value).collect();
        values.sort_by(f64::total_cmp);
        let middle = values.len() / 2;
        match values.len() {
            0 => None,
            length if length % 2 == 1 => values.get(middle).copied(),
            _ => Some((values.get(middle - 1)? + values.get(middle)?) / 2.0),
        }
    }

    /// The largest `value` over the runs.
    pub fn worst(&self, value: impl Fn(&LoadRun) -> Duration) -> Duration {
        self.runs.iter().map(value).max().unwrap_or_default()
    }

    /// Whether every run succeeded with the right answers.
    pub fn clean(&self) -> bool {
        self.failure.is_none() && self.runs.iter().all(LoadRun::clean)
    }
}

/// Every engine's runs of one point.
#[derive(Clone, Debug)]
pub struct PointReport {
    pub point: Point,
    pub engines: Vec<EngineRuns>,
}

/// Runs every point of `load` on every engine, `reps` times each, alternating which engine goes
/// first, each run on a fresh copy of the loaded graph.
pub fn run_load(
    environment: &mut Environment<'_>,
    engines: &[&dyn Engine],
    data: &Dataset,
    load: &Load,
    reps: usize,
    mut progress: impl FnMut(&str),
) -> Vec<PointReport> {
    let answers = Answers::of(data);
    let mut reports = Vec::new();
    for point in Point::all(load) {
        progress(&point.label());
        let mut series: Vec<EngineRuns> = engines
            .iter()
            .map(|engine| EngineRuns {
                engine: engine.kind(),
                runs: Vec::new(),
                failure: None,
            })
            .collect();
        for rep in 0..reps {
            let order: Vec<usize> = if rep % 2 == 0 {
                (0..engines.len()).collect()
            } else {
                (0..engines.len()).rev().collect()
            };
            for index in order {
                let (Some(engine), Some(runs)) = (engines.get(index), series.get_mut(index)) else {
                    continue;
                };
                if runs.failure.is_some() {
                    continue;
                }
                let outcome = environment.loaded_copy(*engine).and_then(|dir| {
                    let outcome =
                        run_point(*engine, &dir, point, &answers, load.duration, rep as u64);
                    let _ = fs::remove_dir_all(&dir);
                    outcome
                });
                match outcome {
                    Ok(run) => runs.runs.push(run),
                    Err(error) => runs.failure = Some(error.to_string()),
                }
            }
        }
        reports.push(PointReport {
            point,
            engines: series,
        });
    }
    reports
}

/// The least share of its one-reader commit throughput that RhizoDB's writer must keep beside
/// more readers.
pub const WRITER_SHARE: f64 = 0.5;

/// Every mixed point where RhizoDB's median commits per second fell below [`WRITER_SHARE`] of
/// its median beside one reader, which means the readers starve the writer.
pub fn starved_writer(reports: &[PointReport]) -> Vec<String> {
    let commits = |report: &PointReport| {
        report
            .engines
            .iter()
            .find(|series| series.engine == EngineKind::Rhizodb)
            .and_then(|series| series.median(LoadRun::write_throughput))
    };
    let mixed = || {
        reports
            .iter()
            .filter(|report| report.point.scenario == Scenario::Mixed)
    };
    let Some(alone) = mixed()
        .find(|report| report.point.readers == 1)
        .and_then(commits)
    else {
        return Vec::new();
    };
    mixed()
        .filter_map(|report| {
            let found = commits(report)?;
            (found < alone * WRITER_SHARE).then(|| {
                format!(
                    "{}: rhizodb committed {found:.0}/s, below {:.0}% of {alone:.0}/s beside one reader",
                    report.point.label(),
                    WRITER_SHARE * 100.0
                )
            })
        })
        .collect()
}

fn micros(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1e6
}

fn show_us(value: Option<f64>) -> String {
    match value {
        None => "-".to_owned(),
        Some(us) if us >= 10_000.0 => format!("{:.1} ms", us / 1000.0),
        Some(us) => format!("{us:.0} us"),
    }
}

/// The table of every point: throughput and latency medians over the runs, the worst stall,
/// and whether every answer was right.
pub fn load_table(reports: &[PointReport]) -> String {
    let mut out = String::from(
        "| point | engine | reads/s | read p50 | read p99 | read max | commits/s | commit p50 | commit p99 | commit max | longest read gap | ckpts | wrong | errors |\n|---|---|---|---|---|---|---|---|---|---|---|---|---|---|\n",
    );
    for report in reports {
        for series in &report.engines {
            if let Some(failure) = &series.failure {
                out.push_str(&format!(
                    "| {} | {} | failed: {failure} |\n",
                    report.point.label(),
                    series.engine.name()
                ));
                continue;
            }
            let reads = report.point.scenario.reads();
            let writes = report.point.scenario.writes();
            let pick = |on: bool, value: Option<f64>| if on { value } else { None };
            let wrong: u64 = series.runs.iter().map(|run| run.wrong).sum();
            let errors: u64 = series.runs.iter().map(|run| run.errors).sum();
            let checkpoints = series.median(|run| run.checkpoints as f64).unwrap_or(0.0);
            out.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {checkpoints:.0} | {wrong} | {errors} |\n",
                report.point.label(),
                series.engine.name(),
                pick(reads, series.median(LoadRun::read_throughput)).map_or("-".to_owned(), |v| format!("{v:.0}")),
                show_us(pick(reads, series.median(|run| micros(run.reads.p50)))),
                show_us(pick(reads, series.median(|run| micros(run.reads.p99)))),
                show_us(pick(reads, Some(micros(series.worst(|run| run.reads.max))))),
                pick(writes, series.median(LoadRun::write_throughput)).map_or("-".to_owned(), |v| format!("{v:.0}")),
                show_us(pick(writes, series.median(|run| micros(run.writes.p50)))),
                show_us(pick(writes, series.median(|run| micros(run.writes.p99)))),
                show_us(pick(writes, Some(micros(series.worst(|run| run.writes.max))))),
                show_us(pick(reads, Some(micros(series.worst(|run| run.longest_read_gap))))),
            ));
        }
    }
    out
}

/// The reports as JSON, every run with its timeline.
pub fn load_json(reports: &[PointReport], header: Value) -> Value {
    json!({
        "header": header,
        "points": reports.iter().map(|report| json!({
            "point": report.point.label(),
            "scenario": report.point.scenario.name(),
            "readers": report.point.readers,
            "engines": report.engines.iter().map(|series| json!({
                "engine": series.engine.name(),
                "failure": series.failure,
                "runs": series.runs.iter().map(LoadRun::json).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
#[path = "stress.test.rs"]
mod tests;
