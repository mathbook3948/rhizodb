//! Random sequences of write transactions run through GQL against a plain in-memory model of
//! the graph, with read queries whose answers the model computes on its own. Shared by the
//! `model_ops` integration test and the `graph_ops` fuzz target.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt::Write as _;

use rhizodb::Database;
use rhizodb::ErrorKind;
use rhizodb::IndexTarget;
use rhizodb::Parameters;
use rhizodb::Value;

/// Where the random choices come from: a seeded generator in tests, the fuzzer's bytes in a
/// fuzz target.
pub trait Choices {
    /// A number in `0..bound`, for a `bound` above zero.
    fn below(&mut self, bound: u64) -> u64;

    /// Whether the run should end, as when the fuzzer's bytes run out.
    fn exhausted(&self) -> bool {
        false
    }
}

/// Reads choices from fuzzer bytes, and then zeros once they run out.
#[derive(Debug)]
pub struct ByteChoices<'a> {
    bytes: &'a [u8],
}

impl<'a> ByteChoices<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }
}

impl Choices for ByteChoices<'_> {
    fn below(&mut self, bound: u64) -> u64 {
        let Some((&first, rest)) = self.bytes.split_first() else {
            return 0;
        };
        self.bytes = rest;
        u64::from(first) % bound
    }

    fn exhausted(&self) -> bool {
        self.bytes.is_empty()
    }
}

const LABELS: [&str; 3] = ["A", "B", "C"];
const STRINGS_OF_FIVE: [&str; 5] = ["0", "1", "2", "3", "4"];
const TYPES: [&str; 2] = ["T", "U"];
const STRINGS: [&str; 4] = ["", "a", "b", "é"];

/// The values of `m`, a property whose type changes from node to node and write to write. No
/// two of them are equal numbers, so grouping and equality do not depend on how GQL compares
/// an integer with a float.
const MIXED: [Mixed; 9] = [
    Mixed::Int(7),
    Mixed::Int(-4),
    Mixed::Float(-0.0),
    Mixed::Float(2.5),
    Mixed::Bool(true),
    Mixed::Bool(false),
    Mixed::Str("m"),
    Mixed::Str(""),
    Mixed::Pair,
];

#[derive(Clone, Copy, Debug, PartialEq)]
enum Mixed {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(&'static str),
    /// The list `[1, 'x']`.
    Pair,
}

impl Mixed {
    fn value(self) -> Value {
        match self {
            Mixed::Int(value) => Value::Integer(value),
            Mixed::Float(value) => Value::Float(value),
            Mixed::Bool(value) => Value::Boolean(value),
            Mixed::Str(value) => Value::String(value.to_owned()),
            Mixed::Pair => Value::List(vec![Value::Integer(1), Value::String("x".to_owned())]),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Number {
    Int(i64),
    Float(f64),
}

impl Number {
    fn as_f64(self) -> f64 {
        match self {
            Number::Int(value) => value as f64,
            Number::Float(value) => value,
        }
    }

    fn value(self) -> Value {
        match self {
            Number::Int(value) => Value::Integer(value),
            Number::Float(value) => Value::Float(value),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct ModelNode {
    labels: BTreeSet<&'static str>,
    p: Option<Number>,
    q: Option<&'static str>,
    m: Option<Mixed>,
}

#[derive(Clone, Debug, PartialEq)]
struct ModelRel {
    start: i64,
    end: i64,
    rel_type: &'static str,
    w: i64,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Model {
    nodes: BTreeMap<i64, ModelNode>,
    rels: BTreeMap<i64, ModelRel>,
}

impl Model {
    fn degree(&self, key: i64) -> usize {
        self.rels
            .values()
            .filter(|rel| rel.start == key || rel.end == key)
            .count()
    }
}

/// One statement of a write transaction, with the change it makes to the model when it
/// succeeds.
#[derive(Clone, Debug)]
enum Step {
    CreateNode(i64, ModelNode),
    SetP(i64, Option<Number>),
    SetQ(i64, Option<&'static str>),
    SetM(i64, Option<Mixed>),
    /// Creates nodes with the keys `first..first + count`, each labelled `A` and with `p` and
    /// `m` chosen by its key, so a bulk insert fills several row groups.
    CreateMany {
        first: i64,
        count: i64,
    },
    /// Sets `p` of every node whose key is a multiple of `stride`.
    SetEveryP {
        stride: i64,
        p: Option<Number>,
    },
    AddLabel(i64, &'static str),
    RemoveLabel(i64, &'static str),
    CreateRel(i64, ModelRel),
    DeleteRel(i64),
    DeleteNode {
        key: i64,
        detach: bool,
    },
}

fn parameters(entries: &[(&str, Value)]) -> Parameters {
    entries
        .iter()
        .map(|(name, value)| ((*name).to_owned(), value.clone()))
        .collect()
}

fn optional(value: Option<Value>) -> Value {
    value.unwrap_or(Value::Null)
}

impl Step {
    fn query(&self) -> (String, Parameters) {
        match self {
            Step::CreateNode(key, node) => {
                let labels: String = node
                    .labels
                    .iter()
                    .map(|label| format!(":{label}"))
                    .collect();
                (
                    format!("INSERT (n{labels} {{key: $key}}) SET n.p = $p, n.q = $q, n.m = $m"),
                    parameters(&[
                        ("key", Value::Integer(*key)),
                        ("p", optional(node.p.map(Number::value))),
                        ("q", optional(node.q.map(|q| Value::String(q.to_owned())))),
                        ("m", optional(node.m.map(Mixed::value))),
                    ]),
                )
            }
            Step::CreateMany { first, count } => (
                "UNWIND range($first, $first + $count - 1) AS i \
                 INSERT (n:A {key: i, p: i % 7 - 3}) \
                 SET n.m = CASE i % 3 WHEN 0 THEN toString(i % 5) WHEN 1 THEN i % 4 = 0 ELSE NULL END"
                    .to_owned(),
                parameters(&[
                    ("first", Value::Integer(*first)),
                    ("count", Value::Integer(*count)),
                ]),
            ),
            Step::SetEveryP { stride, p } => (
                "MATCH (n) WHERE n.key % $stride = 0 SET n.p = $p".to_owned(),
                parameters(&[
                    ("stride", Value::Integer(*stride)),
                    ("p", optional(p.map(Number::value))),
                ]),
            ),
            Step::SetM(key, m) => (
                "MATCH (n {key: $key}) SET n.m = $m".to_owned(),
                parameters(&[
                    ("key", Value::Integer(*key)),
                    ("m", optional(m.map(Mixed::value))),
                ]),
            ),
            Step::SetP(key, p) => (
                "MATCH (n {key: $key}) SET n.p = $p".to_owned(),
                parameters(&[
                    ("key", Value::Integer(*key)),
                    ("p", optional(p.map(Number::value))),
                ]),
            ),
            Step::SetQ(key, q) => (
                "MATCH (n {key: $key}) SET n.q = $q".to_owned(),
                parameters(&[
                    ("key", Value::Integer(*key)),
                    ("q", optional(q.map(|q| Value::String(q.to_owned())))),
                ]),
            ),
            Step::AddLabel(key, label) => (
                format!("MATCH (n {{key: $key}}) SET n:{label}"),
                parameters(&[("key", Value::Integer(*key))]),
            ),
            Step::RemoveLabel(key, label) => (
                format!("MATCH (n {{key: $key}}) REMOVE n:{label}"),
                parameters(&[("key", Value::Integer(*key))]),
            ),
            Step::CreateRel(key, rel) => (
                format!(
                    "MATCH (a {{key: $start}}), (b {{key: $end}}) \
                     INSERT (a)-[:{} {{rk: $key, w: $w}}]->(b)",
                    rel.rel_type
                ),
                parameters(&[
                    ("key", Value::Integer(*key)),
                    ("start", Value::Integer(rel.start)),
                    ("end", Value::Integer(rel.end)),
                    ("w", Value::Integer(rel.w)),
                ]),
            ),
            Step::DeleteRel(key) => (
                "MATCH ()-[r {rk: $key}]->() DELETE r".to_owned(),
                parameters(&[("key", Value::Integer(*key))]),
            ),
            Step::DeleteNode { key, detach } => (
                format!(
                    "MATCH (n {{key: $key}}) {}DELETE n",
                    if *detach { "DETACH " } else { "" }
                ),
                parameters(&[("key", Value::Integer(*key))]),
            ),
        }
    }

    /// Applies the step to the model, or returns the error kind the database must raise.
    fn apply(&self, model: &mut Model) -> Result<(), ErrorKind> {
        match self {
            Step::CreateNode(key, node) => {
                model.nodes.insert(*key, node.clone());
            }
            Step::SetP(key, p) => {
                if let Some(node) = model.nodes.get_mut(key) {
                    node.p = *p;
                }
            }
            Step::SetQ(key, q) => {
                if let Some(node) = model.nodes.get_mut(key) {
                    node.q = *q;
                }
            }
            Step::SetM(key, m) => {
                if let Some(node) = model.nodes.get_mut(key) {
                    node.m = *m;
                }
            }
            Step::CreateMany { first, count } => {
                for key in *first..*first + *count {
                    let m = match key % 3 {
                        0 => Some(Mixed::Str(
                            STRINGS_OF_FIVE[usize::try_from(key % 5).unwrap_or(0)],
                        )),
                        1 => Some(Mixed::Bool(key % 4 == 0)),
                        _ => None,
                    };
                    let node = ModelNode {
                        labels: BTreeSet::from(["A"]),
                        p: Some(Number::Int(key % 7 - 3)),
                        q: None,
                        m,
                    };
                    model.nodes.insert(key, node);
                }
            }
            Step::SetEveryP { stride, p } => {
                for (key, node) in &mut model.nodes {
                    if key % stride == 0 {
                        node.p = *p;
                    }
                }
            }
            Step::AddLabel(key, label) => {
                if let Some(node) = model.nodes.get_mut(key) {
                    node.labels.insert(label);
                }
            }
            Step::RemoveLabel(key, label) => {
                if let Some(node) = model.nodes.get_mut(key) {
                    node.labels.remove(label);
                }
            }
            Step::CreateRel(key, rel) => {
                if model.nodes.contains_key(&rel.start) && model.nodes.contains_key(&rel.end) {
                    model.rels.insert(*key, rel.clone());
                }
            }
            Step::DeleteRel(key) => {
                model.rels.remove(key);
            }
            Step::DeleteNode { key, detach } => {
                if !model.nodes.contains_key(key) {
                    return Ok(());
                }
                if !detach && model.degree(*key) > 0 {
                    return Err(ErrorKind::DeleteConnectedNode);
                }
                model
                    .rels
                    .retain(|_, rel| rel.start != *key && rel.end != *key);
                model.nodes.remove(key);
            }
        }
        Ok(())
    }
}

/// The run's state: the database, the model of what it committed, and the next unused keys.
#[derive(Debug)]
pub struct Run {
    db: Database,
    model: Model,
    next_key: i64,
    indexed: BTreeSet<&'static str>,
    log: String,
}

impl Run {
    pub fn new(db: Database) -> Self {
        Self {
            db,
            model: Model::default(),
            next_key: 0,
            indexed: BTreeSet::new(),
            log: String::new(),
        }
    }

    /// The statements run so far, for a failure message.
    pub fn log(&self) -> &str {
        &self.log
    }

    /// Closes the database and checks the one `open` returns against the model, as after
    /// reopening its file.
    pub fn reopen(&mut self, open: impl FnOnce() -> Database) -> Result<(), String> {
        self.db = Database::in_memory().map_err(|error| error.to_string())?;
        self.db = open();
        let _ = writeln!(self.log, "-- reopen");
        self.check_state(&self.model.clone(), None)
    }

    fn pick_node(next_key: i64, choices: &mut dyn Choices) -> i64 {
        let bound = u64::try_from(next_key.max(1)).unwrap_or(1) + 1;
        i64::try_from(choices.below(bound)).unwrap_or(0)
    }

    fn pick_rel(next_key: i64, model: &Model, choices: &mut dyn Choices) -> i64 {
        let keys: Vec<i64> = model.rels.keys().copied().collect();
        if keys.is_empty() || choices.below(8) == 0 {
            return next_key + 1;
        }
        keys[usize::try_from(choices.below(keys.len() as u64)).unwrap_or(0)]
    }

    fn number(choices: &mut dyn Choices) -> Option<Number> {
        match choices.below(10) {
            0 => None,
            1 => Some(Number::Float(
                f64::from(i32::try_from(choices.below(7)).unwrap_or(0)) - 2.5,
            )),
            _ => Some(Number::Int(
                i64::try_from(choices.below(7)).unwrap_or(0) - 3,
            )),
        }
    }

    fn mixed(choices: &mut dyn Choices) -> Option<Mixed> {
        let index = usize::try_from(choices.below(MIXED.len() as u64 + 1)).unwrap_or(0);
        MIXED.get(index).copied()
    }

    fn step(next_key: &mut i64, model: &Model, choices: &mut dyn Choices) -> Step {
        match choices.below(15) {
            12 => Step::SetM(Self::pick_node(*next_key, choices), Self::mixed(choices)),
            13 => {
                let first = *next_key;
                let count = if choices.below(16) == 0 {
                    4000 + i64::try_from(choices.below(300)).unwrap_or(0)
                } else {
                    1 + i64::try_from(choices.below(64)).unwrap_or(0)
                };
                *next_key += count;
                Step::CreateMany { first, count }
            }
            14 => Step::SetEveryP {
                stride: 1 + i64::try_from(choices.below(4)).unwrap_or(0),
                p: Self::number(choices),
            },
            0..=2 => {
                let key = *next_key;
                *next_key += 1;
                let labels = LABELS
                    .iter()
                    .copied()
                    .filter(|_| choices.below(2) == 0)
                    .collect();
                let q = match choices.below(5) {
                    0 => None,
                    index => Some(STRINGS[usize::try_from(index - 1).unwrap_or(0)]),
                };
                let p = Self::number(choices);
                Step::CreateNode(
                    key,
                    ModelNode {
                        labels,
                        p,
                        q,
                        m: Self::mixed(choices),
                    },
                )
            }
            3 => Step::SetP(Self::pick_node(*next_key, choices), Self::number(choices)),
            4 => {
                let q = match choices.below(5) {
                    0 => None,
                    index => Some(STRINGS[usize::try_from(index - 1).unwrap_or(0)]),
                };
                Step::SetQ(Self::pick_node(*next_key, choices), q)
            }
            5 => Step::AddLabel(
                Self::pick_node(*next_key, choices),
                LABELS[usize::try_from(choices.below(3)).unwrap_or(0)],
            ),
            6 => Step::RemoveLabel(
                Self::pick_node(*next_key, choices),
                LABELS[usize::try_from(choices.below(3)).unwrap_or(0)],
            ),
            7..=9 => {
                let key = *next_key;
                *next_key += 1;
                Step::CreateRel(
                    key,
                    ModelRel {
                        start: Self::pick_node(*next_key, choices),
                        end: Self::pick_node(*next_key, choices),
                        rel_type: TYPES[usize::try_from(choices.below(2)).unwrap_or(0)],
                        w: i64::try_from(choices.below(5)).unwrap_or(0),
                    },
                )
            }
            10 => Step::DeleteRel(Self::pick_rel(*next_key, model, choices)),
            _ => Step::DeleteNode {
                key: Self::pick_node(*next_key, choices),
                detach: choices.below(2) == 0,
            },
        }
    }

    /// Runs one write transaction of random steps, commits or rolls it back, and checks the
    /// database against the model. Returns a description of the first disagreement.
    pub fn transaction(&mut self, choices: &mut dyn Choices) -> Result<(), String> {
        if choices.below(10) == 0 {
            return self.toggle_index(choices);
        }
        let mut working = self.model.clone();
        let mut tx = self.db.begin_write().map_err(|error| error.to_string())?;
        let mut failed = None;
        for _ in 0..=choices.below(5) {
            let step = Self::step(&mut self.next_key, &working, choices);
            let (query, parameters) = step.query();
            let _ = writeln!(self.log, "{query} {parameters:?}");
            let expected = step.apply(&mut working);
            let actual = tx.execute(&query, &parameters).map(|_| ());
            match (expected, actual) {
                (Ok(()), Ok(())) => {}
                (Err(kind), Err(error)) if error.kind() == kind => {
                    failed = Some(kind);
                    break;
                }
                (expected, actual) => {
                    return Err(format!(
                        "{query}: the model expected {expected:?}, the database gave {:?}",
                        actual.map_err(|error| (error.kind(), error.to_string()))
                    ));
                }
            }
            if choices.below(4) == 0 {
                let rows = state_inside(&mut tx)?;
                let want = rows_of(&working);
                if rows != want {
                    return Err(format!(
                        "inside the transaction the database holds\n{rows:#?}\nand the model\n{want:#?}"
                    ));
                }
            }
        }
        let commit = failed.is_none() && choices.below(5) != 0;
        if commit {
            let _ = writeln!(self.log, "-- commit");
            tx.commit().map_err(|error| format!("commit: {error}"))?;
            self.model = working.clone();
        } else {
            let _ = writeln!(self.log, "-- rollback");
            drop(tx);
        }
        self.check_state(&self.model.clone(), Some(choices))
    }

    fn toggle_index(&mut self, choices: &mut dyn Choices) -> Result<(), String> {
        let label = LABELS[usize::try_from(choices.below(3)).unwrap_or(0)];
        let name = format!("p_{label}");
        if self.indexed.contains(label) {
            let _ = writeln!(self.log, "-- drop index {name}");
            self.db
                .drop_index(&name)
                .map_err(|error| error.to_string())?;
            self.indexed.remove(label);
        } else {
            let _ = writeln!(self.log, "-- create index {name}");
            self.db
                .create_property_index(&name, IndexTarget::Label(label), "p")
                .map_err(|error| error.to_string())?;
            self.indexed.insert(label);
        }
        self.check_state(&self.model.clone(), Some(choices))
    }

    fn check_state(&self, model: &Model, choices: Option<&mut dyn Choices>) -> Result<(), String> {
        let read = self.db.begin_read().map_err(|error| error.to_string())?;
        let execute = |query: &str, parameters: &Parameters| {
            read.execute(query, parameters)
                .map(|result| result.rows().to_vec())
                .map_err(|error| format!("{query}: {error}"))
        };
        let rows = dump(&execute)?;
        let want = rows_of(model);
        if rows != want {
            return Err(format!(
                "the database holds\n{rows:#?}\nand the model\n{want:#?}"
            ));
        }
        if let Some(choices) = choices {
            for _ in 0..3 {
                check_query(model, &execute, choices)?;
            }
        }
        Ok(())
    }
}

type Rows = (Vec<Vec<Value>>, Vec<Vec<Value>>);

type Execute<'a> = dyn Fn(&str, &Parameters) -> Result<Vec<Vec<Value>>, String> + 'a;

const NODE_DUMP: &str = "MATCH (n) RETURN n.key, labels(n), n.p, n.q, n.m ORDER BY n.key";
const REL_DUMP: &str = "MATCH (a)-[r]->(b) RETURN r.rk, a.key, type(r), r.w, b.key ORDER BY r.rk";

fn dump(execute: &Execute<'_>) -> Result<Rows, String> {
    let mut nodes = execute(NODE_DUMP, &Parameters::new())?;
    for row in &mut nodes {
        if let Value::List(labels) = &mut row[1] {
            labels.sort_by_key(|label| format!("{label:?}"));
        }
    }
    Ok((nodes, execute(REL_DUMP, &Parameters::new())?))
}

fn state_inside(tx: &mut rhizodb::WriteTransaction<'_>) -> Result<Rows, String> {
    let nodes = tx
        .execute(NODE_DUMP, &Parameters::new())
        .map_err(|error| error.to_string())?;
    let rels = tx
        .execute(REL_DUMP, &Parameters::new())
        .map_err(|error| error.to_string())?;
    let mut nodes = nodes.rows().to_vec();
    for row in &mut nodes {
        if let Value::List(labels) = &mut row[1] {
            labels.sort_by_key(|label| format!("{label:?}"));
        }
    }
    Ok((nodes, rels.rows().to_vec()))
}

fn rows_of(model: &Model) -> Rows {
    let nodes = model
        .nodes
        .iter()
        .map(|(key, node)| {
            vec![
                Value::Integer(*key),
                Value::List(
                    node.labels
                        .iter()
                        .map(|label| Value::String((*label).to_owned()))
                        .collect(),
                ),
                optional(node.p.map(Number::value)),
                optional(node.q.map(|q| Value::String(q.to_owned()))),
                optional(node.m.map(Mixed::value)),
            ]
        })
        .collect();
    let rels = model
        .rels
        .iter()
        .map(|(key, rel)| {
            vec![
                Value::Integer(*key),
                Value::Integer(rel.start),
                Value::String(rel.rel_type.to_owned()),
                Value::Integer(rel.w),
                Value::Integer(rel.end),
            ]
        })
        .collect();
    (nodes, rels)
}

#[derive(Clone, Copy, Debug)]
enum Comparison {
    Eq,
    Lt,
    Ge,
    Ne,
}

impl Comparison {
    const ALL: [Comparison; 4] = [
        Comparison::Eq,
        Comparison::Lt,
        Comparison::Ge,
        Comparison::Ne,
    ];

    fn symbol(self) -> &'static str {
        match self {
            Comparison::Eq => "=",
            Comparison::Lt => "<",
            Comparison::Ge => ">=",
            Comparison::Ne => "<>",
        }
    }

    /// The comparison under GQL's three-valued logic, with `None` for unknown.
    fn holds(self, left: Option<Number>, right: Number) -> Option<bool> {
        let left = left?.as_f64();
        let right = right.as_f64();
        Some(match self {
            Comparison::Eq => left == right,
            Comparison::Lt => left < right,
            Comparison::Ge => left >= right,
            Comparison::Ne => left != right,
        })
    }
}

fn integer(rows: &[Vec<Value>]) -> Option<i64> {
    match rows {
        [row] => match row.as_slice() {
            [Value::Integer(value)] => Some(*value),
            _ => None,
        },
        _ => None,
    }
}

/// Runs one random read query and compares it with the model's answer.
fn check_query(
    model: &Model,
    execute: &Execute<'_>,
    choices: &mut dyn Choices,
) -> Result<(), String> {
    let label = LABELS[usize::try_from(choices.below(3)).unwrap_or(0)];
    let comparison = Comparison::ALL[usize::try_from(choices.below(4)).unwrap_or(0)];
    let constant = match choices.below(4) {
        0 => Number::Float(f64::from(i32::try_from(choices.below(7)).unwrap_or(0)) - 2.5),
        _ => Number::Int(i64::try_from(choices.below(7)).unwrap_or(0) - 3),
    };
    let c = parameters(&[("c", constant.value())]);
    let labelled = || {
        model
            .nodes
            .values()
            .filter(move |node| node.labels.contains(label))
    };
    match choices.below(9) {
        5 => {
            let query = format!(
                "MATCH (n:{label}) RETURN count(n), count(n.p), sum(n.p), min(n.p), max(n.p)"
            );
            let numbers: Vec<Number> = labelled().filter_map(|node| node.p).collect();
            let all_int = numbers
                .iter()
                .all(|number| matches!(number, Number::Int(_)));
            let sum = if all_int {
                Value::Integer(numbers.iter().map(|number| number.as_f64() as i64).sum())
            } else {
                Value::Float(numbers.iter().map(|number| number.as_f64()).sum())
            };
            let by_value = |a: &&Number, b: &&Number| a.as_f64().total_cmp(&b.as_f64());
            let want = vec![vec![
                Value::Integer(labelled().count() as i64),
                Value::Integer(numbers.len() as i64),
                sum,
                optional(numbers.iter().min_by(by_value).map(|number| number.value())),
                optional(numbers.iter().max_by(by_value).map(|number| number.value())),
            ]];
            expect_rows(execute(&query, &Parameters::new())?, want, &query)
        }
        6 => {
            let query = format!("MATCH (n:{label}) RETURN n.q AS q, count(*) AS c");
            let mut groups: BTreeMap<Option<&str>, i64> = BTreeMap::new();
            for node in labelled() {
                *groups.entry(node.q).or_default() += 1;
            }
            let want = groups
                .into_iter()
                .map(|(q, count)| {
                    vec![
                        optional(q.map(|q| Value::String(q.to_owned()))),
                        Value::Integer(count),
                    ]
                })
                .collect();
            expect_rows_in_any_order(execute(&query, &Parameters::new())?, want, &query)
        }
        7 => {
            let query = "MATCH (n) RETURN n.m AS m, count(*) AS c, count(n.p) AS p";
            let mut groups: Vec<(Option<Mixed>, i64, i64)> = Vec::new();
            for node in model.nodes.values() {
                let with_p = i64::from(node.p.is_some());
                match groups.iter_mut().find(|(m, _, _)| *m == node.m) {
                    Some(group) => {
                        group.1 += 1;
                        group.2 += with_p;
                    }
                    None => groups.push((node.m, 1, with_p)),
                }
            }
            let want = groups
                .into_iter()
                .map(|(m, count, p)| {
                    vec![
                        optional(m.map(Mixed::value)),
                        Value::Integer(count),
                        Value::Integer(p),
                    ]
                })
                .collect();
            expect_rows_in_any_order(execute(query, &Parameters::new())?, want, query)
        }
        8 => {
            let query = "MATCH (n) WHERE n.q IS NOT NULL RETURN n.q, n.key ORDER BY n.q DESC, n.key LIMIT 4";
            let mut rows: Vec<(&str, i64)> = model
                .nodes
                .iter()
                .filter_map(|(key, node)| node.q.map(|q| (q, *key)))
                .collect();
            rows.sort_by(|a, b| b.0.cmp(a.0).then(a.1.cmp(&b.1)));
            let want = rows
                .into_iter()
                .take(4)
                .map(|(q, key)| vec![Value::String(q.to_owned()), Value::Integer(key)])
                .collect();
            expect_rows(execute(query, &Parameters::new())?, want, query)
        }
        0 => {
            let query = format!(
                "MATCH (n:{label}) WHERE n.p {} $c RETURN count(n)",
                comparison.symbol()
            );
            let want = labelled()
                .filter(|node| comparison.holds(node.p, constant) == Some(true))
                .count();
            expect_count(execute(&query, &c)?, want, &query, &c)
        }
        1 => {
            let base = format!("MATCH (n:{label}) WHERE ");
            let predicate = format!("n.p {} $c", comparison.symbol());
            let mut total = 0;
            for filter in [
                predicate.clone(),
                format!("NOT ({predicate})"),
                format!("({predicate}) IS NULL"),
            ] {
                let query = format!("{base}{filter} RETURN count(n)");
                total += integer(&execute(&query, &c)?)
                    .ok_or_else(|| format!("{query} returned no count"))?;
            }
            let all = labelled().count();
            if usize::try_from(total).ok() != Some(all) {
                return Err(format!(
                    "ternary partition of {predicate} on :{label} with {c:?} sums to {total}, \
                     but :{label} has {all} nodes"
                ));
            }
            Ok(())
        }
        2 => {
            let rel_type = TYPES[usize::try_from(choices.below(2)).unwrap_or(0)];
            let other = LABELS[usize::try_from(choices.below(3)).unwrap_or(0)];
            let query = format!("MATCH (a:{label})-[:{rel_type}]->(b:{other}) RETURN count(*)");
            let has = |key: &i64, wanted: &str| {
                model
                    .nodes
                    .get(key)
                    .is_some_and(|node| node.labels.contains(wanted))
            };
            let want = model
                .rels
                .values()
                .filter(|rel| {
                    rel.rel_type == rel_type && has(&rel.start, label) && has(&rel.end, other)
                })
                .count();
            expect_count(
                execute(&query, &Parameters::new())?,
                want,
                &query,
                &Parameters::new(),
            )
        }
        3 => {
            let query = "MATCH (a)-[r1]->()-[r2]->(c) RETURN count(*)";
            let want = model
                .rels
                .iter()
                .map(|(k1, r1)| {
                    model
                        .rels
                        .iter()
                        .filter(|(k2, r2)| k1 != *k2 && r2.start == r1.end)
                        .count()
                })
                .sum();
            expect_count(
                execute(query, &Parameters::new())?,
                want,
                query,
                &Parameters::new(),
            )
        }
        _ => {
            let query = format!(
                "MATCH (n:{label}) WHERE n.p IS NOT NULL RETURN n.p ORDER BY n.p, n.key LIMIT 3"
            );
            let mut numbers: Vec<(f64, i64, Number)> = model
                .nodes
                .iter()
                .filter(|(_, node)| node.labels.contains(label))
                .filter_map(|(key, node)| node.p.map(|p| (p.as_f64(), *key, p)))
                .collect();
            numbers.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
            let want: Vec<Vec<Value>> = numbers
                .into_iter()
                .take(3)
                .map(|(_, _, p)| vec![p.value()])
                .collect();
            let got = execute(&query, &Parameters::new())?;
            if got != want {
                return Err(format!(
                    "{query}: the database gave {got:?}, the model {want:?}"
                ));
            }
            Ok(())
        }
    }
}

fn expect_count(
    rows: Vec<Vec<Value>>,
    want: usize,
    query: &str,
    parameters: &Parameters,
) -> Result<(), String> {
    match integer(&rows) {
        Some(got) if usize::try_from(got).ok() == Some(want) => Ok(()),
        _ => Err(format!(
            "{query} with {parameters:?}: the database gave {rows:?}, the model {want}"
        )),
    }
}

fn expect_rows(got: Vec<Vec<Value>>, want: Vec<Vec<Value>>, query: &str) -> Result<(), String> {
    if got == want {
        return Ok(());
    }
    Err(format!(
        "{query}: the database gave {got:?}, the model {want:?}"
    ))
}

fn expect_rows_in_any_order(
    mut got: Vec<Vec<Value>>,
    mut want: Vec<Vec<Value>>,
    query: &str,
) -> Result<(), String> {
    got.sort_by_key(|row| format!("{row:?}"));
    want.sort_by_key(|row| format!("{row:?}"));
    expect_rows(got, want, query)
}
