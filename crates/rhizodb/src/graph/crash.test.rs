use std::collections::BTreeMap;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::path::Path;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::thread::sleep;
use std::time::Duration;
use std::time::Instant;

use super::records::Properties;
use super::*;
use crate::storage::redb_backend::RedbStorage;
use crate::value::PropertyValue;

const DATABASE_ENV: &str = "RHIZODB_CRASH_DATABASE";
const WRITER_ITERATION_CAP: u64 = 100_000;
const READY_LINE: &str = "ready";
const COMMITTED_PREFIX: &str = "committed ";

const TRIALS: u64 = 24;

fn kill_point(trial: u64) -> (u64, u64) {
    (trial * 5 % 34, trial * 613 % 3_000)
}

#[derive(Clone, Copy)]
struct Tokens {
    iter_label: Token<Label>,
    tail_label: Token<Label>,
    done_label: Token<Label>,
    next_relationship: Token<RelationshipType>,
    iter_key: Token<PropertyKey>,
    name_key: Token<PropertyKey>,
    payload_key: Token<PropertyKey>,
    next_key: Token<PropertyKey>,
    at_key: Token<PropertyKey>,
}

impl Tokens {
    fn intern(write: &mut GraphTx<impl WriteTransaction>) -> Self {
        Self {
            iter_label: write.intern_token("Iter").unwrap(),
            tail_label: write.intern_token("Tail").unwrap(),
            done_label: write.intern_token("Done").unwrap(),
            next_relationship: write.intern_token("NEXT").unwrap(),
            iter_key: write.intern_token("iter").unwrap(),
            name_key: write.intern_token("name").unwrap(),
            payload_key: write.intern_token("payload").unwrap(),
            next_key: write.intern_token("next").unwrap(),
            at_key: write.intern_token("at").unwrap(),
        }
    }

    fn lookup(read: &GraphTx<impl ReadTransaction>) -> Option<Self> {
        Some(Self {
            iter_label: read.lookup_token("Iter").unwrap()?,
            tail_label: read.lookup_token("Tail").unwrap()?,
            done_label: read.lookup_token("Done").unwrap()?,
            next_relationship: read.lookup_token("NEXT").unwrap()?,
            iter_key: read.lookup_token("iter").unwrap()?,
            name_key: read.lookup_token("name").unwrap()?,
            payload_key: read.lookup_token("payload").unwrap()?,
            next_key: read.lookup_token("next").unwrap()?,
            at_key: read.lookup_token("at").unwrap()?,
        })
    }
}

fn integer(value: u64) -> PropertyValue {
    PropertyValue::Integer(i64::try_from(value).unwrap())
}

fn head_id(iteration: u64) -> NodeId {
    NodeId(2 * iteration)
}

fn tail_id(iteration: u64) -> NodeId {
    NodeId(2 * iteration + 1)
}

fn link_id(iteration: u64) -> RelationshipId {
    RelationshipId(iteration)
}

fn head_record(tokens: &Tokens, iteration: u64) -> NodeRecord {
    NodeRecord {
        labels: [tokens.iter_label].into(),
        properties: Properties::from([
            (tokens.iter_key, integer(iteration)),
            (
                tokens.name_key,
                PropertyValue::String(format!("head-{iteration}")),
            ),
        ]),
    }
}

fn tail_record(tokens: &Tokens, iteration: u64) -> NodeRecord {
    NodeRecord {
        labels: [tokens.iter_label, tokens.tail_label].into(),
        properties: Properties::from([
            (tokens.iter_key, integer(iteration)),
            (
                tokens.payload_key,
                PropertyValue::List(vec![
                    PropertyValue::Boolean(iteration.is_multiple_of(2)),
                    PropertyValue::String("x".repeat(200)),
                    PropertyValue::Bytes(vec![0xab; 64]),
                    PropertyValue::Float(iteration as f64 / 4.0),
                ]),
            ),
            (
                tokens.at_key,
                PropertyValue::List(vec![
                    PropertyValue::Float(iteration as f64),
                    PropertyValue::Float(1.0),
                ]),
            ),
        ]),
    }
}

fn link_record(tokens: &Tokens, iteration: u64) -> RelationshipRecord {
    RelationshipRecord {
        rel_type: tokens.next_relationship,
        source: head_id(iteration),
        target: tail_id(iteration),
        properties: Properties::from([(tokens.iter_key, integer(iteration))]),
    }
}

fn write_iteration(graph: &Graph<RedbStorage>, iteration: u64) {
    let mut write = graph.write().unwrap();
    let tokens = Tokens::intern(&mut write);
    if iteration == 0 {
        write
            .create_keyword_index("words", IndexTarget::Label("Iter"), &["name", "payload"])
            .unwrap();
        write
            .create_vector_index(
                "points",
                IndexTarget::Label("Tail"),
                "at",
                2,
                VectorMetric::Cosine,
            )
            .unwrap();
    }
    assert_eq!(
        write.create_node(&head_record(&tokens, iteration)).unwrap(),
        head_id(iteration)
    );
    assert_eq!(
        write.create_node(&tail_record(&tokens, iteration)).unwrap(),
        tail_id(iteration)
    );
    assert_eq!(
        write
            .create_relationship(&link_record(&tokens, iteration))
            .unwrap(),
        link_id(iteration)
    );
    if let Some(previous) = iteration.checked_sub(1) {
        write
            .set_node_property(head_id(previous), tokens.next_key, integer(iteration))
            .unwrap();
        write
            .remove_node_property(head_id(previous), tokens.name_key)
            .unwrap();
        write
            .add_label(head_id(previous), tokens.done_label)
            .unwrap();
        write.delete_relationship(link_id(previous)).unwrap();
        write.delete_node(tail_id(previous)).unwrap();
    }
    write.commit().unwrap();
}

#[derive(Debug, PartialEq)]
struct State {
    nodes: BTreeMap<NodeId, NodeRecord>,
    relationships: BTreeMap<RelationshipId, RelationshipRecord>,
}

fn expected_state(tokens: &Tokens, committed: u64) -> State {
    let mut state = State {
        nodes: BTreeMap::new(),
        relationships: BTreeMap::new(),
    };
    for iteration in 0..committed {
        let mut head = head_record(tokens, iteration);
        state.nodes.insert(head_id(iteration), head.clone());
        if iteration + 1 == committed {
            state
                .nodes
                .insert(tail_id(iteration), tail_record(tokens, iteration));
            state
                .relationships
                .insert(link_id(iteration), link_record(tokens, iteration));
        } else {
            head.properties.remove(&tokens.name_key);
            head.properties
                .insert(tokens.next_key, integer(iteration + 1));
            head.labels.insert(tokens.done_label);
            state.nodes.insert(head_id(iteration), head);
        }
    }
    state
}

fn actual_state(read: &GraphTx<impl ReadTransaction>) -> State {
    let mut state = State {
        nodes: BTreeMap::new(),
        relationships: BTreeMap::new(),
    };
    for id in read.node_ids().unwrap() {
        state.nodes.insert(id, read.node(id).unwrap().unwrap());
        for adjacent in read.adjacent(id, Direction::Outgoing, None).unwrap() {
            let record = read.relationship(adjacent.relationship).unwrap().unwrap();
            state.relationships.insert(adjacent.relationship, record);
        }
    }
    state
}

fn first_differences<K: Ord + Copy + std::fmt::Debug, V: PartialEq + std::fmt::Debug>(
    expected: &BTreeMap<K, V>,
    actual: &BTreeMap<K, V>,
) -> Vec<String> {
    let keys: std::collections::BTreeSet<K> =
        expected.keys().chain(actual.keys()).copied().collect();
    keys.into_iter()
        .filter(|key| expected.get(key) != actual.get(key))
        .take(4)
        .map(|key| {
            format!(
                "{key:?}: expected {:?}, found {:?}",
                expected.get(&key),
                actual.get(&key)
            )
        })
        .collect()
}

fn recover(path: &Path) -> (Graph<RedbStorage>, Result<u64, String>) {
    let graph = Graph::open(RedbStorage::open(path).unwrap()).unwrap();
    let verdict = verify(&graph);
    (graph, verdict)
}

fn verify(graph: &Graph<RedbStorage>) -> Result<u64, String> {
    let read = graph.read().unwrap();
    let violations = read.check_integrity().unwrap();
    if !violations.is_empty() {
        return Err(format!("integrity violations: {violations:?}"));
    }
    let committed = read
        .node_ids()
        .unwrap()
        .into_iter()
        .max()
        .map_or(0, |highest| highest.0 / 2 + 1);
    let actual = actual_state(&read);
    let expected = match Tokens::lookup(&read) {
        Some(tokens) => expected_state(&tokens, committed),
        None if committed == 0 && actual.nodes.is_empty() => return Ok(0),
        None => return Err(format!("{committed} iterations present without tokens")),
    };
    if actual == expected {
        return Ok(committed);
    }
    Err(format!(
        "state is not exactly {committed} whole iterations. nodes: {:?} relationships: {:?}",
        first_differences(&expected.nodes, &actual.nodes),
        first_differences(&expected.relationships, &actual.relationships),
    ))
}

#[test]
#[ignore = "child process of the crash test, selected by DATABASE_ENV"]
fn crash_writer() {
    let Ok(path) = std::env::var(DATABASE_ENV) else {
        return;
    };
    let graph = Graph::open(RedbStorage::open(path).unwrap()).unwrap();
    let mut stdout = std::io::stdout();
    writeln!(stdout, "{READY_LINE}").unwrap();
    stdout.flush().unwrap();
    for iteration in 0..WRITER_ITERATION_CAP {
        write_iteration(&graph, iteration);
        writeln!(stdout, "{COMMITTED_PREFIX}{iteration}").unwrap();
        stdout.flush().unwrap();
    }
}

type Damage = fn(&mut GraphTx<<RedbStorage as Storage>::Write<'_>>, &Tokens);

struct Writer {
    child: Child,
    stdout: BufReader<std::process::ChildStdout>,
}

impl Writer {
    fn spawn(path: &Path) -> Self {
        let name = format!("{}::crash_writer", module_path!());
        let name = name.strip_prefix("rhizodb::").unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([name, "--exact", "--ignored", "--nocapture"])
            .env(DATABASE_ENV, path)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Self { child, stdout }
    }

    fn next_line(&mut self) -> Option<String> {
        let mut line = String::new();
        let read = self.stdout.read_line(&mut line).unwrap_or(0);
        (read > 0 && line.ends_with('\n')).then(|| line.trim_end().to_owned())
    }

    fn next_committed(&mut self) -> Option<u64> {
        loop {
            let line = self.next_line()?;
            if let Some(number) = line.strip_prefix(COMMITTED_PREFIX) {
                return Some(number.parse().unwrap());
            }
        }
    }

    fn wait_until_ready(&mut self) {
        loop {
            let line = self.next_line().expect("writer exited before it was ready");
            if line == READY_LINE {
                return;
            }
        }
    }

    fn kill_and_collect_committed(mut self) -> Vec<u64> {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
        std::iter::from_fn(|| self.next_committed()).collect()
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
    }
}

#[test]
fn the_oracle_accepts_whole_iterations_and_continues_the_sequence() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("oracle.redb");
    let graph = Graph::open(RedbStorage::open(&path).unwrap()).unwrap();
    assert_eq!(verify(&graph), Ok(0));
    for iteration in 0..4 {
        write_iteration(&graph, iteration);
        assert_eq!(verify(&graph), Ok(iteration + 1));
    }
}

#[test]
fn the_oracle_rejects_a_partially_applied_iteration() {
    let tear_down: [Damage; 5] = [
        |write, _| write.delete_relationship(link_id(2)).unwrap(),
        |write, tokens| {
            write
                .set_node_property(head_id(1), tokens.next_key, integer(99))
                .unwrap();
        },
        |write, tokens| {
            write.remove_label(head_id(1), tokens.done_label).unwrap();
        },
        |write, _| {
            write
                .transaction
                .delete(Table::Nodes, &keys::element_key(tail_id(2).0))
                .unwrap();
        },
        |write, _| {
            write
                .transaction
                .delete(Table::Vectors, &keys::index_entry_key(1, tail_id(2).0))
                .unwrap();
        },
    ];
    for damage in tear_down {
        let graph = Graph::open(RedbStorage::in_memory().unwrap()).unwrap();
        for iteration in 0..3 {
            write_iteration(&graph, iteration);
        }
        assert_eq!(verify(&graph), Ok(3));
        let mut write = graph.write().unwrap();
        let tokens = Tokens::intern(&mut write);
        damage(&mut write, &tokens);
        write.commit().unwrap();
        assert!(verify(&graph).is_err());
    }
}

#[test]
fn a_database_survives_the_writer_being_killed_at_varied_points() {
    let started = Instant::now();
    let directory = tempfile::tempdir().unwrap();
    for trial in 0..TRIALS {
        let (lines_before_kill, delay_micros) = kill_point(trial);
        let path = directory.path().join(format!("trial-{trial}.redb"));
        let mut writer = Writer::spawn(&path);
        writer.wait_until_ready();
        let mut reported: Vec<u64> = (0..lines_before_kill)
            .map(|_| writer.next_committed().expect("writer exited early"))
            .collect();
        sleep(Duration::from_micros(delay_micros));
        reported.extend(writer.kill_and_collect_committed());
        let expected_sequence: Vec<u64> = (0..reported.len() as u64).collect();
        assert_eq!(
            reported, expected_sequence,
            "trial {trial}: reports skipped"
        );

        let (graph, verdict) = recover(&path);
        let recovered = verdict.unwrap_or_else(|reason| panic!("trial {trial}: {reason}"));
        let reported_count = reported.len() as u64;
        assert!(
            recovered >= reported_count,
            "trial {trial}: reported {reported_count} committed but {recovered} survived"
        );
        println!(
            "trial {trial}: killed after {lines_before_kill} lines and {delay_micros}us, \
             reported {reported_count} committed, recovered {recovered}"
        );

        write_iteration(&graph, recovered);
        assert_eq!(
            verify(&graph),
            Ok(recovered + 1),
            "trial {trial}: after a new write"
        );
        drop(graph);
        let (_, reopened) = recover(&path);
        assert_eq!(
            reopened,
            Ok(recovered + 1),
            "trial {trial}: after reopening"
        );
    }
    println!("crash test took {:?}", started.elapsed());
}
