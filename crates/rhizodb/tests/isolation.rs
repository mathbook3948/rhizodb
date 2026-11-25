//! Concurrent random transactions over list-append registers, checked for isolation anomalies
//! the way Elle checks a history (Kingsbury and Alvaro, "Elle: Inferring Isolation Anomalies
//! from Experimental Observations", VLDB 2020).
//!
//! Every key is a `:K` node holding a list. A write transaction appends unique values to some
//! keys, and every append also inserts one `:V` node, so a read that sees the list and the `:V`
//! nodes of a key in one snapshot also checks that a commit is visible across records at once.
//! The final lists give the version order of each key, and from it the checker derives
//! write-write, write-read, and read-write dependencies, adds real-time order, and requires the
//! graph to be acyclic. The transactions doc promises one writer at a time and snapshot reads,
//! which together make the history strictly serializable, so any cycle is a bug. A cycle names
//! lost updates, write skew, and read skew alike; aborted and intermediate reads are checked
//! value by value.

mod support;

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::Barrier;
use std::thread;
use std::time::Instant;

use rhizodb::Database;
use rhizodb::ErrorKind;
use rhizodb::Parameters;
use rhizodb::Value;
use support::*;

const KEYS: i64 = 6;
const THREADS: u64 = 6;
const FAST_TRANSACTIONS_PER_THREAD: u64 = 40;

#[derive(Clone, Debug)]
enum Op {
    Read { key: i64, list: Vec<i64> },
    Append { key: i64, value: i64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Committed,
    Aborted,
}

#[derive(Clone, Debug)]
struct Txn {
    id: usize,
    thread: u64,
    style: &'static str,
    start: Instant,
    end: Instant,
    ops: Vec<Op>,
    outcome: Outcome,
}

impl Txn {
    fn appends(&self) -> impl Iterator<Item = (i64, i64)> + '_ {
        self.ops.iter().filter_map(|op| match op {
            Op::Append { key, value } => Some((*key, *value)),
            Op::Read { .. } => None,
        })
    }
}

fn key_parameters(key: i64) -> Parameters {
    Parameters::from([("k".to_owned(), Value::Integer(key))])
}

fn integers(value: &Value) -> Vec<i64> {
    match value {
        Value::List(items) => items
            .iter()
            .map(|item| match item {
                Value::Integer(value) => *value,
                other => panic!("a list item is {other:?}"),
            })
            .collect(),
        other => panic!("a list is {other:?}"),
    }
}

const READ_LIST: &str = "MATCH (n:K {k: $k}) RETURN n.l";
const READ_RECORDS: &str = "MATCH (v:V {k: $k}) RETURN v.v";
const APPEND_IN_QUERY: &str = "MATCH (n:K {k: $k}) SET n.l = n.l + [$v] INSERT (:V {k: $k, v: $v})";
const SET_LIST: &str = "MATCH (n:K {k: $k}) SET n.l = $l INSERT (:V {k: $k, v: $v})";
const AUTOCOMMIT_APPEND: &str =
    "MATCH (n:K {k: $k}) SET n.l = n.l + [$v] INSERT (:V {k: $k, v: $v}) RETURN n.l";

type Execute<'a> = dyn FnMut(&str, &Parameters) -> Result<Vec<Vec<Value>>, rhizodb::Error> + 'a;

/// Reads a key's list and checks that the `:V` nodes of the same snapshot hold the same values.
fn read_key(execute: &mut Execute<'_>, key: i64) -> Vec<i64> {
    let parameters = key_parameters(key);
    let rows = execute(READ_LIST, &parameters).unwrap();
    assert_eq!(rows.len(), 1, "key {key} has {} nodes", rows.len());
    let list = integers(&rows[0][0]);
    let records: BTreeSet<i64> = execute(READ_RECORDS, &parameters)
        .unwrap()
        .iter()
        .map(|row| match row[0] {
            Value::Integer(value) => value,
            ref other => panic!("a :V value is {other:?}"),
        })
        .collect();
    assert_eq!(
        list.iter().copied().collect::<BTreeSet<_>>(),
        records,
        "key {key}: one snapshot shows the list {list:?} but the :V nodes {records:?}"
    );
    list
}

struct Worker {
    db: Arc<Database>,
    thread: u64,
    rng: Rng,
    next_value: i64,
}

impl Worker {
    fn value(&mut self) -> i64 {
        self.next_value += 1;
        i64::try_from(self.thread).unwrap() * 1_000_000 + self.next_value
    }

    fn key(&mut self) -> i64 {
        i64::try_from(self.rng.below(KEYS as u64)).unwrap()
    }

    fn transaction(&mut self) -> (Instant, Instant, &'static str, Vec<Op>, Outcome) {
        match self.rng.below(10) {
            0..=2 => self.read_transaction(),
            3 => self.autocommit_append(),
            _ => self.write_transaction(),
        }
    }

    fn read_transaction(&mut self) -> (Instant, Instant, &'static str, Vec<Op>, Outcome) {
        let db = Arc::clone(&self.db);
        let start = Instant::now();
        let tx = db.begin_read().unwrap();
        let mut ops = Vec::new();
        for _ in 0..=self.rng.below(4) {
            let key = self.key();
            let list = read_key(
                &mut |query, parameters| tx.execute(query, parameters).map(|r| r.rows().to_vec()),
                key,
            );
            ops.push(Op::Read { key, list });
            if self.rng.chance(1, 4) {
                thread::yield_now();
            }
        }
        drop(tx);
        (start, Instant::now(), "read", ops, Outcome::Committed)
    }

    fn autocommit_append(&mut self) -> (Instant, Instant, &'static str, Vec<Op>, Outcome) {
        let key = self.key();
        let value = self.value();
        let mut parameters = key_parameters(key);
        parameters.insert("v".to_owned(), Value::Integer(value));
        let start = Instant::now();
        let result = self.db.execute(AUTOCOMMIT_APPEND, &parameters).unwrap();
        let end = Instant::now();
        let after = integers(&result.rows()[0][0]);
        assert_eq!(after.last(), Some(&value), "the append returned {after:?}");
        let before = after[..after.len() - 1].to_vec();
        (
            start,
            end,
            "autocommit",
            vec![Op::Read { key, list: before }, Op::Append { key, value }],
            Outcome::Committed,
        )
    }

    fn write_transaction(&mut self) -> (Instant, Instant, &'static str, Vec<Op>, Outcome) {
        let db = Arc::clone(&self.db);
        let start = Instant::now();
        let mut tx = db.begin_write().unwrap();
        let mut ops = Vec::new();
        for _ in 0..=self.rng.below(4) {
            let key = self.key();
            match self.rng.below(3) {
                0 => {
                    let list = read_key(
                        &mut |query, parameters| {
                            tx.execute(query, parameters).map(|r| r.rows().to_vec())
                        },
                        key,
                    );
                    ops.push(Op::Read { key, list });
                }
                1 => {
                    let value = self.value();
                    let mut parameters = key_parameters(key);
                    parameters.insert("v".to_owned(), Value::Integer(value));
                    tx.execute(APPEND_IN_QUERY, &parameters).unwrap();
                    ops.push(Op::Append { key, value });
                }
                _ => {
                    let mut list = read_key(
                        &mut |query, parameters| {
                            tx.execute(query, parameters).map(|r| r.rows().to_vec())
                        },
                        key,
                    );
                    ops.push(Op::Read {
                        key,
                        list: list.clone(),
                    });
                    let value = self.value();
                    list.push(value);
                    let mut parameters = key_parameters(key);
                    parameters.insert("v".to_owned(), Value::Integer(value));
                    parameters.insert(
                        "l".to_owned(),
                        Value::List(list.into_iter().map(Value::Integer).collect()),
                    );
                    tx.execute(SET_LIST, &parameters).unwrap();
                    ops.push(Op::Append { key, value });
                }
            }
            if self.rng.chance(1, 4) {
                thread::yield_now();
            }
        }
        let outcome = match self.rng.below(10) {
            0 => {
                tx.rollback().unwrap();
                Outcome::Aborted
            }
            1 => {
                drop(tx);
                Outcome::Aborted
            }
            2 => {
                let error = tx.execute("RETURN 1 / 0", &Parameters::new()).unwrap_err();
                assert_eq!(error.kind(), ErrorKind::DivisionByZero);
                assert_eq!(
                    tx.commit().unwrap_err().kind(),
                    ErrorKind::TransactionAborted
                );
                Outcome::Aborted
            }
            _ => {
                tx.commit().unwrap();
                Outcome::Committed
            }
        };
        (start, Instant::now(), "write", ops, outcome)
    }
}

fn run_history(db: &Arc<Database>, seed: u64, per_thread: u64) -> Vec<Txn> {
    let barrier = Arc::new(Barrier::new(THREADS as usize));
    let handles: Vec<_> = (0..THREADS)
        .map(|thread| {
            let db = Arc::clone(db);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let mut worker = Worker {
                    db,
                    thread,
                    rng: Rng::new(seed ^ (thread + 1).wrapping_mul(0x2545_f491_4f6c_dd1d)),
                    next_value: 0,
                };
                barrier.wait();
                (0..per_thread)
                    .map(|_| {
                        let (start, end, style, ops, outcome) = worker.transaction();
                        (thread, start, end, style, ops, outcome)
                    })
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    handles
        .into_iter()
        .flat_map(|handle| handle.join().unwrap())
        .enumerate()
        .map(|(id, (thread, start, end, style, ops, outcome))| Txn {
            id,
            thread,
            style,
            start,
            end,
            ops,
            outcome,
        })
        .collect()
}

fn final_lists(db: &Database) -> BTreeMap<i64, Vec<i64>> {
    let read = db.begin_read().unwrap();
    (0..KEYS)
        .map(|key| {
            let list = read_key(
                &mut |query, parameters| read.execute(query, parameters).map(|r| r.rows().to_vec()),
                key,
            );
            (key, list)
        })
        .collect()
}

#[derive(Debug, Default)]
struct Graph {
    edges: Vec<BTreeMap<usize, &'static str>>,
}

impl Graph {
    fn add(&mut self, from: usize, to: usize, why: &'static str) {
        if from != to {
            self.edges[from].entry(to).or_insert(why);
        }
    }

    fn cycle(&self, members: &[usize]) -> Option<Vec<(usize, &'static str)>> {
        let included: BTreeSet<usize> = members.iter().copied().collect();
        let mut state = vec![0u8; self.edges.len()];
        let mut parent: Vec<Option<(usize, &'static str)>> = vec![None; self.edges.len()];
        for &root in members {
            if state[root] != 0 {
                continue;
            }
            let mut stack = vec![(root, self.edges[root].iter())];
            state[root] = 1;
            while let Some((node, iter)) = stack.last_mut() {
                let node = *node;
                match iter.next() {
                    Some((&next, &why)) if included.contains(&next) => match state[next] {
                        0 => {
                            state[next] = 1;
                            parent[next] = Some((node, why));
                            stack.push((next, self.edges[next].iter()));
                        }
                        1 => {
                            let mut cycle = vec![(node, why)];
                            let mut at = node;
                            while at != next {
                                let (previous, why) = parent[at].unwrap();
                                cycle.push((previous, why));
                                at = previous;
                            }
                            cycle.reverse();
                            return Some(cycle);
                        }
                        _ => {}
                    },
                    Some(_) => {}
                    None => {
                        state[node] = 2;
                        stack.pop();
                    }
                }
            }
        }
        None
    }
}

/// Checks the history and returns every anomaly it finds, described for a human.
fn anomalies(history: &[Txn], finals: &BTreeMap<i64, Vec<i64>>) -> Vec<String> {
    let mut found = Vec::new();
    let mut writer_of: HashMap<i64, usize> = HashMap::new();
    let mut key_of: HashMap<i64, i64> = HashMap::new();
    for txn in history {
        for (key, value) in txn.appends() {
            writer_of.insert(value, txn.id);
            key_of.insert(value, key);
        }
    }
    let committed = |value: &i64| history[writer_of[value]].outcome == Outcome::Committed;

    for (key, list) in finals {
        let distinct: BTreeSet<_> = list.iter().collect();
        if distinct.len() != list.len() {
            found.push(format!(
                "key {key}: the final list {list:?} repeats a value"
            ));
        }
        for value in list {
            match writer_of.get(value) {
                None => found.push(format!("key {key}: {value} was never written")),
                Some(_) if !committed(value) => found.push(format!(
                    "G1a: key {key} holds {value} of aborted transaction {}",
                    writer_of[value]
                )),
                Some(_) if key_of[value] != *key => {
                    found.push(format!("key {key} holds {value}, written to another key"))
                }
                Some(_) => {}
            }
        }
    }
    for txn in history
        .iter()
        .filter(|txn| txn.outcome == Outcome::Committed)
    {
        for (key, value) in txn.appends() {
            if !finals[&key].contains(&value) {
                found.push(format!(
                    "lost update: committed transaction {} appended {value} to key {key}, \
                     and the final list {:?} lacks it",
                    txn.id, finals[&key]
                ));
            }
        }
    }

    let position: HashMap<i64, usize> = finals
        .values()
        .flat_map(|list| {
            list.iter()
                .enumerate()
                .map(|(index, value)| (*value, index))
        })
        .collect();
    let mut graph = Graph {
        edges: vec![BTreeMap::new(); history.len()],
    };
    for list in finals.values() {
        for pair in list.windows(2) {
            graph.add(writer_of[&pair[0]], writer_of[&pair[1]], "ww");
        }
    }

    for txn in history {
        let mut own: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
        for op in &txn.ops {
            match op {
                Op::Append { key, value } => own.entry(*key).or_default().push(*value),
                Op::Read { key, list } => {
                    let own_values = own.get(key).cloned().unwrap_or_default();
                    let split = list.len().saturating_sub(own_values.len());
                    if list[split..] != own_values[..] {
                        found.push(format!(
                            "transaction {} read {list:?} from key {key} without its own \
                             appends {own_values:?} at the end",
                            txn.id
                        ));
                        continue;
                    }
                    let external = &list[..split];
                    for value in external {
                        if !writer_of.contains_key(value) {
                            found.push(format!(
                                "transaction {} read {value}, which nobody wrote",
                                txn.id
                            ));
                        } else if !committed(value) {
                            found.push(format!(
                                "G1a: transaction {} read {value} of aborted transaction {}",
                                txn.id, writer_of[value]
                            ));
                        }
                    }
                    let final_list = &finals[key];
                    if final_list.get(..external.len()) != Some(external) {
                        found.push(format!(
                            "transaction {} read {external:?} from key {key}, which is not a \
                             prefix of its final list {final_list:?}",
                            txn.id
                        ));
                        continue;
                    }
                    if txn.outcome != Outcome::Committed {
                        continue;
                    }
                    if let Some(last) = external.last() {
                        let writer = writer_of[last];
                        let unseen = txn_appends_after(history, writer, *key, &position, *last);
                        if unseen {
                            found.push(format!(
                                "G1b: transaction {} read {last} of transaction {writer} \
                                 without that transaction's later appends to key {key}",
                                txn.id
                            ));
                        }
                        graph.add(writer, txn.id, "wr");
                    }
                    if let Some(next) = final_list.get(external.len()) {
                        graph.add(txn.id, writer_of[next], "rw");
                    }
                }
            }
        }
    }

    let mut members: Vec<usize> = history
        .iter()
        .filter(|txn| txn.outcome == Outcome::Committed)
        .map(|txn| txn.id)
        .collect();
    let mut by_end = members.clone();
    by_end.sort_by_key(|id| history[*id].end);
    let clock = |rank: usize| history.len() + rank;
    graph
        .edges
        .resize(history.len() + by_end.len(), BTreeMap::new());
    for (rank, &id) in by_end.iter().enumerate() {
        graph.add(id, clock(rank), "realtime");
        if rank + 1 < by_end.len() {
            graph.add(clock(rank), clock(rank + 1), "realtime");
        }
    }
    for &later in &members {
        let ended_before = by_end.partition_point(|id| history[*id].end < history[later].start);
        if ended_before > 0 {
            graph.add(clock(ended_before - 1), later, "realtime");
        }
    }
    members.extend((0..by_end.len()).map(clock));
    if let Some(cycle) = graph.cycle(&members) {
        let mut text = String::from("dependency cycle (not strictly serializable):");
        for (id, why) in cycle {
            let Some(txn) = history.get(id) else {
                continue;
            };
            let _ = write!(
                text,
                "\n  T{id} ({} on thread {}) {:?} -{why}->",
                txn.style, txn.thread, txn.ops
            );
        }
        found.push(text);
    }
    found
}

/// Whether the writer of `value` appended to `key` after `value` too, which a reader that saw
/// `value` as the last item must then also see.
fn txn_appends_after(
    history: &[Txn],
    writer: usize,
    key: i64,
    position: &HashMap<i64, usize>,
    value: i64,
) -> bool {
    history[writer]
        .appends()
        .any(|(other_key, other)| other_key == key && position.get(&other) > position.get(&value))
}

fn check(db: Arc<Database>, seed: u64, per_thread: u64) -> BTreeMap<i64, Vec<i64>> {
    for key in 0..KEYS {
        db.execute("INSERT (:K {k: $k, l: []})", &key_parameters(key))
            .unwrap();
    }
    let history = run_history(&db, seed, per_thread);
    let finals = final_lists(&db);
    let found = anomalies(&history, &finals);
    assert!(
        found.is_empty(),
        "seed {seed} (replay with RHIZODB_TEST_SEED={seed}):\n{}",
        found.join("\n")
    );
    let committed = history
        .iter()
        .filter(|txn| txn.outcome == Outcome::Committed && txn.appends().next().is_some())
        .count();
    assert!(committed > 0, "seed {seed}: no write committed");
    finals
}

#[test]
fn concurrent_transactions_in_memory_are_strictly_serializable() {
    let seed = seed();
    let _ = check(
        Arc::new(Database::in_memory().unwrap()),
        seed,
        cases(FAST_TRANSACTIONS_PER_THREAD),
    );
}

#[test]
fn concurrent_transactions_on_a_file_are_strictly_serializable() {
    let seed = seed();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("isolation.rhizodb");
    let finals = check(
        Arc::new(Database::open(&path).unwrap()),
        seed,
        cases(FAST_TRANSACTIONS_PER_THREAD),
    );
    let reopened = Database::open(&path).unwrap();
    assert_eq!(
        final_lists(&reopened),
        finals,
        "seed {seed}: reopening changed the lists"
    );
}

fn txn(id: usize, ops: Vec<Op>, start: Instant, end: Instant) -> Txn {
    Txn {
        id,
        thread: 0,
        style: "write",
        start,
        end,
        ops,
        outcome: Outcome::Committed,
    }
}

#[test]
fn the_checker_reports_a_lost_update() {
    let now = Instant::now();
    let history = vec![
        txn(
            0,
            vec![
                Op::Read {
                    key: 0,
                    list: vec![],
                },
                Op::Append { key: 0, value: 1 },
            ],
            now,
            now,
        ),
        txn(
            1,
            vec![
                Op::Read {
                    key: 0,
                    list: vec![],
                },
                Op::Append { key: 0, value: 2 },
            ],
            now,
            now,
        ),
    ];
    let finals = BTreeMap::from([(0, vec![1, 2])]);
    let found = anomalies(&history, &finals);
    assert!(
        found.iter().any(|anomaly| anomaly.contains("cycle")),
        "{found:?}"
    );
}

#[test]
fn the_checker_reports_write_skew() {
    let now = Instant::now();
    let history = vec![
        txn(
            0,
            vec![
                Op::Read {
                    key: 1,
                    list: vec![],
                },
                Op::Append { key: 0, value: 1 },
            ],
            now,
            now,
        ),
        txn(
            1,
            vec![
                Op::Read {
                    key: 0,
                    list: vec![],
                },
                Op::Append { key: 1, value: 2 },
            ],
            now,
            now,
        ),
    ];
    let finals = BTreeMap::from([(0, vec![1]), (1, vec![2])]);
    let found = anomalies(&history, &finals);
    assert!(
        found.iter().any(|anomaly| anomaly.contains("cycle")),
        "{found:?}"
    );
}

#[test]
fn the_checker_reports_a_stale_read_after_a_commit_returned() {
    let first = Instant::now();
    let later = first + std::time::Duration::from_millis(1);
    let history = vec![
        txn(0, vec![Op::Append { key: 0, value: 1 }], first, first),
        txn(
            1,
            vec![Op::Read {
                key: 0,
                list: vec![],
            }],
            later,
            later,
        ),
    ];
    let finals = BTreeMap::from([(0, vec![1])]);
    let found = anomalies(&history, &finals);
    assert!(
        found.iter().any(|anomaly| anomaly.contains("cycle")),
        "{found:?}"
    );
}

#[test]
fn the_checker_reports_an_aborted_read_and_an_intermediate_read() {
    let now = Instant::now();
    let mut aborted = txn(0, vec![Op::Append { key: 0, value: 1 }], now, now);
    aborted.outcome = Outcome::Aborted;
    let history = vec![
        aborted,
        txn(
            1,
            vec![
                Op::Append { key: 1, value: 2 },
                Op::Append { key: 1, value: 3 },
            ],
            now,
            now,
        ),
        txn(
            2,
            vec![
                Op::Read {
                    key: 0,
                    list: vec![1],
                },
                Op::Read {
                    key: 1,
                    list: vec![2],
                },
            ],
            now,
            now,
        ),
    ];
    let finals = BTreeMap::from([(0, vec![]), (1, vec![2, 3])]);
    let found = anomalies(&history, &finals);
    assert!(
        found.iter().any(|anomaly| anomaly.starts_with("G1a")),
        "{found:?}"
    );
    assert!(
        found.iter().any(|anomaly| anomaly.starts_with("G1b")),
        "{found:?}"
    );
}

#[test]
fn the_checker_accepts_a_serial_history() {
    let first = Instant::now();
    let later = first + std::time::Duration::from_millis(1);
    let history = vec![
        txn(
            0,
            vec![
                Op::Read {
                    key: 0,
                    list: vec![],
                },
                Op::Append { key: 0, value: 1 },
                Op::Read {
                    key: 0,
                    list: vec![1],
                },
            ],
            first,
            first,
        ),
        txn(
            1,
            vec![
                Op::Read {
                    key: 0,
                    list: vec![1],
                },
                Op::Append { key: 0, value: 2 },
            ],
            later,
            later,
        ),
    ];
    let finals = BTreeMap::from([(0, vec![1, 2])]);
    assert_eq!(anomalies(&history, &finals), Vec::<String>::new());
}
