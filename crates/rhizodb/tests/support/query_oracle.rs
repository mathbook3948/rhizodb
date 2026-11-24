//! A random property graph with a plain model of it, random typed GQL expressions over its
//! elements, and a reference evaluator for those expressions under openCypher's three-valued
//! logic, equality, ordering, and grouping. Shared by the query oracle test and its fuzz target.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt::Write as _;

use rhizodb::Database;
use rhizodb::Parameters;
use rhizodb::Value;

pub trait Choices {
    fn below(&mut self, bound: u64) -> u64;

    fn chance(&mut self, numerator: u64, denominator: u64) -> bool {
        self.below(denominator) < numerator
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T
    where
        Self: Sized,
    {
        &items[self.below(items.len() as u64) as usize]
    }
}

#[derive(Clone, Debug)]
pub enum V {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    List(Vec<V>),
}

impl V {
    pub fn value(&self) -> Value {
        match self {
            V::Null => Value::Null,
            V::Bool(value) => Value::Boolean(*value),
            V::Int(value) => Value::Integer(*value),
            V::Float(value) => Value::Float(*value),
            V::Str(value) => Value::String(value.clone()),
            V::List(items) => Value::List(items.iter().map(V::value).collect()),
        }
    }

    fn literal(&self) -> String {
        match self {
            V::Null => "null".to_owned(),
            V::Bool(value) => value.to_string(),
            V::Int(value) if *value < 0 => format!("({value})"),
            V::Int(value) => value.to_string(),
            V::Float(value) if value.is_nan() => "(0.0 / 0.0)".to_owned(),
            V::Float(value) if *value == 0.0 && value.is_sign_negative() => "(-0.0)".to_owned(),
            V::Float(value) if *value < 0.0 => format!("({value:?})"),
            V::Float(value) => format!("{value:?}"),
            V::Str(value) => format!("'{value}'"),
            V::List(items) => {
                let items: Vec<String> = items.iter().map(V::literal).collect();
                format!("[{}]", items.join(", "))
            }
        }
    }

    fn is_null(&self) -> bool {
        matches!(self, V::Null)
    }
}

/// Whether two result values are the same, with every NaN the same and `-0.0` apart from `0.0`.
pub fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Float(a), Value::Float(b)) => {
            a.to_bits() == b.to_bits() || (a.is_nan() && b.is_nan())
        }
        (Value::List(a), Value::List(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| same(a, b))
        }
        _ => a == b,
    }
}

fn number(value: &V) -> Option<f64> {
    match value {
        V::Int(value) => Some(*value as f64),
        V::Float(value) => Some(*value),
        _ => None,
    }
}

pub fn equals(a: &V, b: &V) -> Option<bool> {
    match (a, b) {
        (V::Null, _) | (_, V::Null) => None,
        (V::List(a), V::List(b)) => {
            if a.len() != b.len() {
                return Some(false);
            }
            let mut unknown = false;
            for (a, b) in a.iter().zip(b) {
                match equals(a, b) {
                    Some(false) => return Some(false),
                    None => unknown = true,
                    Some(true) => {}
                }
            }
            if unknown { None } else { Some(true) }
        }
        (V::Int(a), V::Int(b)) => Some(a == b),
        (V::Bool(a), V::Bool(b)) => Some(a == b),
        (V::Str(a), V::Str(b)) => Some(a == b),
        _ => match (number(a), number(b)) {
            (Some(a), Some(b)) => Some(a == b),
            _ => Some(false),
        },
    }
}

/// `None` is null, `Some(None)` is unordered (NaN), which makes every inequality false.
pub fn compare(a: &V, b: &V) -> Option<Option<Ordering>> {
    match (a, b) {
        (V::Null, _) | (_, V::Null) => None,
        (V::Str(a), V::Str(b)) => Some(Some(a.cmp(b))),
        (V::Bool(a), V::Bool(b)) => Some(Some(a.cmp(b))),
        (V::Int(a), V::Int(b)) => Some(Some(a.cmp(b))),
        (V::List(a), V::List(b)) => {
            for (x, y) in a.iter().zip(b) {
                match compare(x, y)? {
                    Some(Ordering::Equal) => {}
                    decided => return Some(decided),
                }
            }
            Some(Some(a.len().cmp(&b.len())))
        }
        _ => {
            let (a, b) = (number(a)?, number(b)?);
            Some(a.partial_cmp(&b))
        }
    }
}

fn rank(value: &V) -> u8 {
    match value {
        V::List(_) => 0,
        V::Str(_) => 1,
        V::Bool(_) => 2,
        V::Int(_) | V::Float(_) => 3,
        V::Null => 4,
    }
}

/// The `ORDER BY` order, which also decides `DISTINCT` and grouping.
pub fn order(a: &V, b: &V) -> Ordering {
    let ranked = rank(a).cmp(&rank(b));
    if ranked != Ordering::Equal {
        return ranked;
    }
    match (a, b) {
        (V::List(a), V::List(b)) => {
            for (x, y) in a.iter().zip(b) {
                let item = order(x, y);
                if item != Ordering::Equal {
                    return item;
                }
            }
            a.len().cmp(&b.len())
        }
        (V::Str(a), V::Str(b)) => a.cmp(b),
        (V::Bool(a), V::Bool(b)) => a.cmp(b),
        (V::Int(a), V::Int(b)) => a.cmp(b),
        _ => match (number(a), number(b)) {
            (Some(a), Some(b)) => match (a.is_nan(), b.is_nan()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => a.partial_cmp(&b).unwrap_or(Ordering::Equal),
            },
            _ => Ordering::Equal,
        },
    }
}

pub const LABELS: [&str; 3] = ["A", "B", "C"];
pub const TYPES: [&str; 2] = ["R", "S"];
const STRINGS: [&str; 7] = ["", "a", "ab", "b", "B", "é", "a\u{e9}"];

#[derive(Clone, Debug)]
pub struct NodeM {
    pub k: i64,
    pub labels: BTreeSet<&'static str>,
    pub props: BTreeMap<&'static str, V>,
}

impl NodeM {
    fn prop(&self, key: &str) -> V {
        self.props.get(key).cloned().unwrap_or(V::Null)
    }
}

#[derive(Clone, Debug)]
pub struct RelM {
    pub k: i64,
    pub src: i64,
    pub dst: i64,
    pub ty: &'static str,
    pub props: BTreeMap<&'static str, V>,
}

impl RelM {
    fn prop(&self, key: &str) -> V {
        self.props.get(key).cloned().unwrap_or(V::Null)
    }
}

#[derive(Clone, Debug, Default)]
pub struct Graph {
    pub nodes: BTreeMap<i64, NodeM>,
    pub rels: BTreeMap<i64, RelM>,
}

impl Graph {
    pub fn has_loop(&self) -> bool {
        self.rels.values().any(|rel| rel.src == rel.dst)
    }
}

pub fn int_value(choices: &mut impl Choices) -> V {
    V::Int(choices.below(7) as i64 - 3)
}

pub fn str_value(choices: &mut impl Choices) -> V {
    V::Str((*choices.pick(&STRINGS)).to_owned())
}

pub fn mixed_value(choices: &mut impl Choices) -> V {
    match choices.below(12) {
        0 | 1 => int_value(choices),
        2 => V::Float(1.5),
        3 => V::Float(-0.0),
        4 => V::Float(f64::NAN),
        5 => V::Float(2.0),
        6 => V::Float(-2.5),
        7 | 8 => str_value(choices),
        9 => V::Bool(choices.chance(1, 2)),
        10 => V::List(vec![int_value(choices), int_value(choices)]),
        _ => V::List(vec![int_value(choices), str_value(choices)]),
    }
}

pub fn node_props(choices: &mut impl Choices) -> BTreeMap<&'static str, V> {
    let mut props = BTreeMap::new();
    if choices.chance(3, 4) {
        props.insert("x", int_value(choices));
    }
    if choices.chance(3, 4) {
        props.insert("y", str_value(choices));
    }
    if choices.chance(2, 3) {
        props.insert("z", mixed_value(choices));
    }
    if choices.chance(1, 2) {
        props.insert("t", V::Bool(choices.chance(1, 2)));
    }
    props
}

pub fn rel_props(choices: &mut impl Choices) -> BTreeMap<&'static str, V> {
    let mut props = BTreeMap::new();
    if choices.chance(3, 4) {
        props.insert("w", int_value(choices));
    }
    props
}

fn props_literal(k: i64, props: &BTreeMap<&'static str, V>) -> String {
    let mut text = format!("{{k: {k}");
    for (key, value) in props {
        let _ = write!(text, ", {key}: {}", value.literal());
    }
    text.push('}');
    text
}

/// Anything that runs a query: a database, or an open write transaction.
pub trait Runner {
    fn run(&mut self, query: &str) -> Result<Vec<Vec<Value>>, String>;
}

impl Runner for &Database {
    fn run(&mut self, query: &str) -> Result<Vec<Vec<Value>>, String> {
        self.execute(query, &Parameters::new())
            .map(|result| result.rows().to_vec())
            .map_err(|error| format!("{query}: {error} ({:?})", error.kind()))
    }
}

impl Runner for rhizodb::WriteTransaction<'_> {
    fn run(&mut self, query: &str) -> Result<Vec<Vec<Value>>, String> {
        self.execute(query, &Parameters::new())
            .map(|result| result.rows().to_vec())
            .map_err(|error| format!("{query}: {error} ({:?})", error.kind()))
    }
}

/// Random edits to apply to the database and the model alike.
const BULK_Z: [V; 6] = [
    V::Int(1),
    V::Float(1.5),
    V::Float(f64::NAN),
    V::Bool(true),
    V::Float(-0.0),
    V::Int(-2),
];

/// Edits that touch many elements at once: a bulk insert, or a `SET`, `REMOVE`, or `DELETE` of
/// every element a random predicate holds for, with values computed from each element.
pub fn bulk_edit(graph: &mut Graph, next_key: &mut i64, choices: &mut impl Choices) -> String {
    let scope = Scope {
        nodes: vec!["n"],
        rels: vec![],
        vars: 0,
    };
    match choices.below(8) {
        0 | 1 => {
            let first = *next_key;
            let count = 1 + choices.below(150) as i64;
            *next_key += count;
            let label = *choices.pick(&LABELS);
            for k in first..first + count {
                let mut props = BTreeMap::new();
                props.insert("x", V::Int(k % 7 - 3));
                props.insert("y", V::Str(STRINGS[(k % 7) as usize].to_owned()));
                if k % 5 != 0 {
                    props.insert("z", BULK_Z[(k % 6) as usize].clone());
                }
                graph.nodes.insert(
                    k,
                    NodeM {
                        k,
                        labels: BTreeSet::from([label]),
                        props,
                    },
                );
            }
            let strings: Vec<String> = STRINGS
                .iter()
                .map(|s| V::Str((*s).to_owned()).literal())
                .collect();
            let zs: Vec<String> = BULK_Z.iter().map(V::literal).collect();
            format!(
                "UNWIND range({first}, {}) AS i INSERT (:{label} {{k: i, x: i % 7 - 3, y: [{}][i % 7], \
                 z: CASE WHEN i % 5 = 0 THEN null ELSE [{}][i % 6] END}})",
                first + count - 1,
                strings.join(", "),
                zs.join(", ")
            )
        }
        2 | 3 => {
            let p = expr(Ty::Bool, 2, &scope, choices);
            let key = *choices.pick(&["x", "y", "z"]);
            let ty = match key {
                "x" => Ty::Int,
                "y" => Ty::Str,
                _ => Ty::Any,
            };
            let value = expr(ty, 2, &scope, choices);
            for node in graph.nodes.values_mut() {
                let row = Row {
                    nodes: vec![node],
                    rels: vec![],
                };
                if matches!(eval(&p, &row), V::Bool(true)) {
                    let new = eval(&value, &row);
                    if new.is_null() {
                        node.props.remove(key);
                    } else {
                        node.props.insert(key, new);
                    }
                }
            }
            format!(
                "MATCH (n) WHERE {} SET n.{key} = {}",
                render(&p, &scope),
                render(&value, &scope)
            )
        }
        4 => {
            let p = expr(Ty::Bool, 2, &scope, choices);
            let label = *choices.pick(&LABELS);
            let add = choices.chance(1, 2);
            for node in graph.nodes.values_mut() {
                if matches!(
                    eval(
                        &p,
                        &Row {
                            nodes: vec![node],
                            rels: vec![]
                        }
                    ),
                    V::Bool(true)
                ) {
                    if add {
                        node.labels.insert(label);
                    } else {
                        node.labels.remove(label);
                    }
                }
            }
            let verb = if add { "SET" } else { "REMOVE" };
            format!("MATCH (n) WHERE {} {verb} n:{label}", render(&p, &scope))
        }
        5 => {
            let p = expr(Ty::Bool, 2, &scope, choices);
            let key = *choices.pick(&["x", "y", "z", "t"]);
            for node in graph.nodes.values_mut() {
                if matches!(
                    eval(
                        &p,
                        &Row {
                            nodes: vec![node],
                            rels: vec![]
                        }
                    ),
                    V::Bool(true)
                ) {
                    node.props.remove(key);
                }
            }
            format!("MATCH (n) WHERE {} REMOVE n.{key}", render(&p, &scope))
        }
        6 => {
            let p = expr(Ty::Bool, 2, &scope, choices);
            let gone: BTreeSet<i64> = graph
                .nodes
                .values()
                .filter(|node| {
                    matches!(
                        eval(
                            &p,
                            &Row {
                                nodes: vec![node],
                                rels: vec![]
                            }
                        ),
                        V::Bool(true)
                    )
                })
                .map(|node| node.k)
                .collect();
            graph.nodes.retain(|k, _| !gone.contains(k));
            graph
                .rels
                .retain(|_, rel| !gone.contains(&rel.src) && !gone.contains(&rel.dst));
            format!("MATCH (n) WHERE {} DETACH DELETE n", render(&p, &scope))
        }
        _ => {
            let scope = Scope {
                nodes: vec!["a", "b"],
                rels: vec!["r"],
                vars: 0,
            };
            let q = expr(Ty::Bool, 2, &scope, choices);
            let value = expr(Ty::Int, 2, &scope, choices);
            let delete = choices.chance(1, 3);
            let mut gone = Vec::new();
            for rel in graph.rels.values() {
                let row = Row {
                    nodes: vec![&graph.nodes[&rel.src], &graph.nodes[&rel.dst]],
                    rels: vec![rel],
                };
                if matches!(eval(&q, &row), V::Bool(true)) {
                    gone.push((rel.k, eval(&value, &row)));
                }
            }
            for (k, new) in gone {
                if delete {
                    graph.rels.remove(&k);
                } else if let Some(rel) = graph.rels.get_mut(&k) {
                    if new.is_null() {
                        rel.props.remove("w");
                    } else {
                        rel.props.insert("w", new);
                    }
                }
            }
            if delete {
                format!("MATCH (a)-[r]->(b) WHERE {} DELETE r", render(&q, &scope))
            } else {
                format!(
                    "MATCH (a)-[r]->(b) WHERE {} SET r.w = {}",
                    render(&q, &scope),
                    render(&value, &scope)
                )
            }
        }
    }
}

pub fn edit(graph: &mut Graph, next_key: &mut i64, choices: &mut impl Choices) -> String {
    if choices.chance(1, 6) {
        return bulk_edit(graph, next_key, choices);
    }
    let node_keys: Vec<i64> = graph.nodes.keys().copied().collect();
    let rel_keys: Vec<i64> = graph.rels.keys().copied().collect();
    let kind = choices.below(10);
    if node_keys.is_empty() || kind < 3 {
        let k = *next_key;
        *next_key += 1;
        let labels: BTreeSet<&'static str> = LABELS
            .iter()
            .copied()
            .filter(|_| choices.chance(1, 2))
            .collect();
        let props = node_props(choices);
        let label_text: String = labels.iter().map(|label| format!(":{label}")).collect();
        let query = format!("INSERT ({label_text} {})", props_literal(k, &props));
        graph.nodes.insert(k, NodeM { k, labels, props });
        return query;
    }
    let node = *choices.pick(&node_keys);
    match kind {
        3 | 4 => {
            let k = *next_key;
            *next_key += 1;
            let other = *choices.pick(&node_keys);
            let ty = *choices.pick(&TYPES);
            let props = rel_props(choices);
            let query = format!(
                "MATCH (a {{k: {node}}}), (b {{k: {other}}}) INSERT (a)-[:{ty} {}]->(b)",
                props_literal(k, &props)
            );
            graph.rels.insert(
                k,
                RelM {
                    k,
                    src: node,
                    dst: other,
                    ty,
                    props,
                },
            );
            query
        }
        5 => {
            let key = *choices.pick(&["x", "y", "z", "t"]);
            let value = match key {
                "x" => int_value(choices),
                "y" => str_value(choices),
                "z" => mixed_value(choices),
                _ => V::Bool(choices.chance(1, 2)),
            };
            let value = if choices.chance(1, 5) { V::Null } else { value };
            let query = format!("MATCH (n {{k: {node}}}) SET n.{key} = {}", value.literal());
            let props = &mut graph.nodes.get_mut(&node).unwrap().props;
            if value.is_null() {
                props.remove(key);
            } else {
                props.insert(key, value);
            }
            query
        }
        6 => {
            let label = *choices.pick(&LABELS);
            let labels = &mut graph.nodes.get_mut(&node).unwrap().labels;
            if choices.chance(1, 2) {
                labels.insert(label);
                format!("MATCH (n {{k: {node}}}) SET n:{label}")
            } else {
                labels.remove(label);
                format!("MATCH (n {{k: {node}}}) REMOVE n:{label}")
            }
        }
        7 if !rel_keys.is_empty() => {
            let rel = *choices.pick(&rel_keys);
            if choices.chance(1, 2) {
                graph.rels.remove(&rel);
                format!("MATCH ()-[r {{k: {rel}}}]->() DELETE r")
            } else {
                let value = if choices.chance(1, 4) {
                    V::Null
                } else {
                    int_value(choices)
                };
                let props = &mut graph.rels.get_mut(&rel).unwrap().props;
                if value.is_null() {
                    props.remove("w");
                } else {
                    props.insert("w", value.clone());
                }
                format!(
                    "MATCH ()-[r {{k: {rel}}}]->() SET r.w = {}",
                    value.literal()
                )
            }
        }
        8 => {
            graph.nodes.remove(&node);
            graph
                .rels
                .retain(|_, rel| rel.src != node && rel.dst != node);
            format!("MATCH (n {{k: {node}}}) DETACH DELETE n")
        }
        _ => {
            let key = *choices.pick(&["x", "y", "z", "t"]);
            graph.nodes.get_mut(&node).unwrap().props.remove(key);
            format!("MATCH (n {{k: {node}}}) REMOVE n.{key}")
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ty {
    Int,
    Str,
    Bool,
    Any,
    List,
}

#[derive(Clone, Debug)]
pub enum E {
    Lit(V),
    NodeProp(usize, &'static str),
    RelProp(usize, &'static str),
    RelType(usize),
    Label(usize, &'static str),
    Arith(&'static str, Box<E>, Box<E>),
    Neg(Box<E>),
    Size(Box<E>),
    Upper(Box<E>),
    Concat(Box<E>, Box<E>),
    Cmp(&'static str, Box<E>, Box<E>),
    Logic(&'static str, Box<E>, Box<E>),
    Not(Box<E>),
    IsNull(Box<E>, bool),
    In(Box<E>, Vec<V>),
    Text(&'static str, Box<E>, Box<E>),
    Case(Box<E>, Box<E>, Box<E>),
    Coalesce(Box<E>, Box<E>),
    Var(usize),
    List(Vec<E>),
    Range(i64, i64),
    Index(Box<E>, Box<E>),
    Slice(Box<E>, i64, i64),
    ListConcat(Box<E>, Box<E>),
    Tail(Box<E>),
    SizeList(Box<E>),
    Comprehension(usize, Box<E>, Option<Box<E>>, Box<E>),
    Quantifier(&'static str, usize, Box<E>, Box<E>),
    Lower(Box<E>),
    Reverse(Box<E>),
    Substring(Box<E>, i64, i64),
}

/// The variables an expression may read: node variables by name and relationship variables by
/// name, both indexed by position.
#[derive(Clone, Debug)]
pub struct Scope {
    pub nodes: Vec<&'static str>,
    pub rels: Vec<&'static str>,
    pub vars: usize,
}

impl Scope {
    fn with_var(&self) -> Scope {
        Scope {
            vars: self.vars + 1,
            ..self.clone()
        }
    }
}

pub struct Row<'a> {
    pub nodes: Vec<&'a NodeM>,
    pub rels: Vec<&'a RelM>,
}

pub fn expr(ty: Ty, depth: u32, scope: &Scope, choices: &mut impl Choices) -> E {
    let leaf = depth == 0 || choices.chance(1, 3);
    match ty {
        Ty::Int => {
            if leaf {
                return match choices.below(5) {
                    0 if !scope.rels.is_empty() => {
                        E::RelProp(choices.below(scope.rels.len() as u64) as usize, "w")
                    }
                    0..=2 => E::NodeProp(choices.below(scope.nodes.len() as u64) as usize, "x"),
                    3 if choices.chance(1, 4) => E::Lit(V::Null),
                    3 if scope.vars > 0 => E::Var(choices.below(scope.vars as u64) as usize),
                    _ => E::Lit(int_value(choices)),
                };
            }
            match choices.below(9) {
                0 => E::Arith(
                    "+",
                    bx(expr(Ty::Int, depth - 1, scope, choices)),
                    bx(expr(Ty::Int, depth - 1, scope, choices)),
                ),
                1 => E::Arith(
                    "-",
                    bx(expr(Ty::Int, depth - 1, scope, choices)),
                    bx(expr(Ty::Int, depth - 1, scope, choices)),
                ),
                2 => E::Arith(
                    "*",
                    bx(expr(Ty::Int, depth - 1, scope, choices)),
                    bx(expr(Ty::Int, depth - 1, scope, choices)),
                ),
                3 => E::Neg(bx(expr(Ty::Int, depth - 1, scope, choices))),
                4 => E::Size(bx(expr(Ty::Str, depth - 1, scope, choices))),
                5 => E::SizeList(bx(expr(Ty::List, depth - 1, scope, choices))),
                6 => E::Index(
                    bx(expr(Ty::List, depth - 1, scope, choices)),
                    bx(expr(Ty::Int, depth - 1, scope, choices)),
                ),
                _ => case_or_coalesce(Ty::Int, depth, scope, choices),
            }
        }
        Ty::Str => {
            if leaf {
                return match choices.below(5) {
                    0 if !scope.rels.is_empty() => {
                        E::RelType(choices.below(scope.rels.len() as u64) as usize)
                    }
                    0..=2 => E::NodeProp(choices.below(scope.nodes.len() as u64) as usize, "y"),
                    3 if choices.chance(1, 4) => E::Lit(V::Null),
                    _ => E::Lit(str_value(choices)),
                };
            }
            match choices.below(7) {
                3 => E::Lower(bx(expr(Ty::Str, depth - 1, scope, choices))),
                4 => E::Reverse(bx(expr(Ty::Str, depth - 1, scope, choices))),
                5 => E::Substring(
                    bx(expr(Ty::Str, depth - 1, scope, choices)),
                    choices.below(3) as i64,
                    choices.below(3) as i64,
                ),
                0 | 1 => E::Concat(
                    bx(expr(Ty::Str, depth - 1, scope, choices)),
                    bx(expr(Ty::Str, depth - 1, scope, choices)),
                ),
                2 => E::Upper(bx(expr(Ty::Str, depth - 1, scope, choices))),
                _ => case_or_coalesce(Ty::Str, depth, scope, choices),
            }
        }
        Ty::Bool => {
            if leaf {
                return match choices.below(6) {
                    0 => E::Label(
                        choices.below(scope.nodes.len() as u64) as usize,
                        choices.pick(&LABELS),
                    ),
                    1 => E::NodeProp(choices.below(scope.nodes.len() as u64) as usize, "t"),
                    2 => E::Lit(match choices.below(3) {
                        0 => V::Bool(true),
                        1 => V::Bool(false),
                        _ => V::Null,
                    }),
                    _ => comparison(0, scope, choices),
                };
            }
            match choices.below(10) {
                0..=2 => E::Logic(
                    choices.pick(&["AND", "OR", "XOR"]),
                    bx(expr(Ty::Bool, depth - 1, scope, choices)),
                    bx(expr(Ty::Bool, depth - 1, scope, choices)),
                ),
                3 => E::Not(bx(expr(Ty::Bool, depth - 1, scope, choices))),
                4 => E::IsNull(
                    bx(expr(Ty::Any, depth - 1, scope, choices)),
                    choices.chance(1, 2),
                ),
                5 => {
                    let items = (0..choices.below(4))
                        .map(|_| {
                            if choices.chance(1, 6) {
                                V::Null
                            } else {
                                mixed_value(choices)
                            }
                        })
                        .collect();
                    E::In(bx(expr(Ty::Any, depth - 1, scope, choices)), items)
                }
                6 => E::Text(
                    choices.pick(&["STARTS WITH", "ENDS WITH", "CONTAINS"]),
                    bx(expr(Ty::Str, depth - 1, scope, choices)),
                    bx(expr(Ty::Str, depth - 1, scope, choices)),
                ),
                7 => case_or_coalesce(Ty::Bool, depth, scope, choices),
                8 if choices.chance(1, 2) => {
                    let inner = scope.with_var();
                    E::Quantifier(
                        choices.pick(&["any", "all", "none", "single"]),
                        scope.vars,
                        bx(expr(Ty::List, depth - 1, scope, choices)),
                        bx(expr(Ty::Bool, depth - 1, &inner, choices)),
                    )
                }
                _ => comparison(depth - 1, scope, choices),
            }
        }
        Ty::Any => match choices.below(5) {
            0 | 1 => E::NodeProp(choices.below(scope.nodes.len() as u64) as usize, "z"),
            2 => expr(Ty::Int, depth, scope, choices),
            3 => expr(Ty::Str, depth, scope, choices),
            _ => expr(Ty::Bool, depth, scope, choices),
        },
        Ty::List => {
            if leaf {
                return match choices.below(4) {
                    0 => E::Range(choices.below(4) as i64 - 1, choices.below(5) as i64 - 1),
                    1 if choices.chance(1, 4) => E::Lit(V::Null),
                    _ => E::List(
                        (0..choices.below(4))
                            .map(|_| expr(Ty::Int, 0, scope, choices))
                            .collect(),
                    ),
                };
            }
            match choices.below(6) {
                0 => E::ListConcat(
                    bx(expr(Ty::List, depth - 1, scope, choices)),
                    bx(expr(Ty::List, depth - 1, scope, choices)),
                ),
                1 => E::Tail(bx(expr(Ty::List, depth - 1, scope, choices))),
                2 => E::Slice(
                    bx(expr(Ty::List, depth - 1, scope, choices)),
                    choices.below(5) as i64 - 2,
                    choices.below(5) as i64 - 1,
                ),
                3 | 4 => {
                    let inner = scope.with_var();
                    let filter = if choices.chance(1, 2) {
                        Some(bx(expr(Ty::Bool, depth - 1, &inner, choices)))
                    } else {
                        None
                    };
                    E::Comprehension(
                        scope.vars,
                        bx(expr(Ty::List, depth - 1, scope, choices)),
                        filter,
                        bx(expr(Ty::Int, depth - 1, &inner, choices)),
                    )
                }
                _ => E::List(
                    (0..choices.below(4))
                        .map(|_| expr(Ty::Int, depth - 1, scope, choices))
                        .collect(),
                ),
            }
        }
    }
}

fn bx(expression: E) -> Box<E> {
    Box::new(expression)
}

fn case_or_coalesce(ty: Ty, depth: u32, scope: &Scope, choices: &mut impl Choices) -> E {
    if choices.chance(1, 2) {
        E::Case(
            bx(expr(Ty::Bool, depth - 1, scope, choices)),
            bx(expr(ty, depth - 1, scope, choices)),
            bx(expr(ty, depth - 1, scope, choices)),
        )
    } else {
        E::Coalesce(
            bx(expr(ty, depth - 1, scope, choices)),
            bx(expr(ty, depth - 1, scope, choices)),
        )
    }
}

fn comparison(depth: u32, scope: &Scope, choices: &mut impl Choices) -> E {
    let op = *choices.pick(&["=", "<>", "<", "<=", ">", ">="]);
    let (left, right) = match choices.below(3) {
        0 => (
            expr(Ty::Int, depth, scope, choices),
            expr(Ty::Int, depth, scope, choices),
        ),
        1 => (
            expr(Ty::Str, depth, scope, choices),
            expr(Ty::Str, depth, scope, choices),
        ),
        _ => {
            let z = E::NodeProp(choices.below(scope.nodes.len() as u64) as usize, "z");
            let other = if choices.chance(1, 2) {
                E::Lit(mixed_value(choices))
            } else {
                expr(Ty::Any, depth, scope, choices)
            };
            if choices.chance(1, 2) {
                (z, other)
            } else {
                (other, z)
            }
        }
    };
    E::Cmp(op, bx(left), bx(right))
}

pub fn render(expression: &E, scope: &Scope) -> String {
    match expression {
        E::Lit(value) => value.literal(),
        E::NodeProp(index, key) => format!("{}.{key}", scope.nodes[*index]),
        E::RelProp(index, key) => format!("{}.{key}", scope.rels[*index]),
        E::RelType(index) => format!("type({})", scope.rels[*index]),
        E::Label(index, label) => format!("({}:{label})", scope.nodes[*index]),
        E::Arith(op, a, b) => format!("({} {op} {})", render(a, scope), render(b, scope)),
        E::Neg(a) => format!("(-{})", render(a, scope)),
        E::Size(a) => format!("size({})", render(a, scope)),
        E::Upper(a) => format!("toUpper({})", render(a, scope)),
        E::Concat(a, b) => format!("({} + {})", render(a, scope), render(b, scope)),
        E::Cmp(op, a, b) => format!("({} {op} {})", render(a, scope), render(b, scope)),
        E::Logic(op, a, b) => format!("({} {op} {})", render(a, scope), render(b, scope)),
        E::Not(a) => format!("(NOT {})", render(a, scope)),
        E::IsNull(a, true) => format!("({} IS NULL)", render(a, scope)),
        E::IsNull(a, false) => format!("({} IS NOT NULL)", render(a, scope)),
        E::In(a, items) => format!(
            "({} IN {})",
            render(a, scope),
            V::List(items.clone()).literal()
        ),
        E::Text(op, a, b) => format!("({} {op} {})", render(a, scope), render(b, scope)),
        E::Case(c, a, b) => format!(
            "(CASE WHEN {} THEN {} ELSE {} END)",
            render(c, scope),
            render(a, scope),
            render(b, scope)
        ),
        E::Coalesce(a, b) => format!("coalesce({}, {})", render(a, scope), render(b, scope)),
        E::Var(index) => format!("x{index}"),
        E::List(items) => {
            let items: Vec<String> = items.iter().map(|item| render(item, scope)).collect();
            format!("[{}]", items.join(", "))
        }
        E::Range(a, b) => format!("range({}, {})", V::Int(*a).literal(), V::Int(*b).literal()),
        E::Index(l, i) => format!("({})[{}]", render(l, scope), render(i, scope)),
        E::Slice(l, a, b) => format!("({})[{a}..{b}]", render(l, scope)),
        E::ListConcat(a, b) => format!("({} + {})", render(a, scope), render(b, scope)),
        E::Tail(l) => format!("tail({})", render(l, scope)),
        E::SizeList(l) => format!("size({})", render(l, scope)),
        E::Comprehension(var, l, filter, projection) => {
            let filter = filter.as_ref().map_or(String::new(), |filter| {
                format!(" WHERE {}", render(filter, scope))
            });
            format!(
                "[x{var} IN {}{filter} | {}]",
                render(l, scope),
                render(projection, scope)
            )
        }
        E::Quantifier(kind, var, l, p) => {
            format!(
                "{kind}(x{var} IN {} WHERE {})",
                render(l, scope),
                render(p, scope)
            )
        }
        E::Lower(a) => format!("toLower({})", render(a, scope)),
        E::Reverse(a) => format!("reverse({})", render(a, scope)),
        E::Substring(a, start, length) => {
            format!("substring({}, {start}, {length})", render(a, scope))
        }
    }
}

fn truth(value: &V) -> Option<bool> {
    match value {
        V::Bool(value) => Some(*value),
        _ => None,
    }
}

fn from_truth(value: Option<bool>) -> V {
    value.map_or(V::Null, V::Bool)
}

pub fn eval(expression: &E, row: &Row<'_>) -> V {
    eval_env(expression, row, &mut Vec::new())
}

fn eval_env(expression: &E, row: &Row<'_>, env: &mut Vec<V>) -> V {
    match expression {
        E::Lit(value) => value.clone(),
        E::NodeProp(index, key) => row.nodes[*index].prop(key),
        E::RelProp(index, key) => row.rels[*index].prop(key),
        E::RelType(index) => V::Str(row.rels[*index].ty.to_owned()),
        E::Label(index, label) => V::Bool(row.nodes[*index].labels.contains(label)),
        E::Arith(op, a, b) => match (eval_env(a, row, env), eval_env(b, row, env)) {
            (V::Int(a), V::Int(b)) => V::Int(match *op {
                "+" => a + b,
                "-" => a - b,
                _ => a * b,
            }),
            _ => V::Null,
        },
        E::Neg(a) => match eval_env(a, row, env) {
            V::Int(a) => V::Int(-a),
            _ => V::Null,
        },
        E::Size(a) => match eval_env(a, row, env) {
            V::Str(a) => V::Int(a.chars().count() as i64),
            _ => V::Null,
        },
        E::Upper(a) => match eval_env(a, row, env) {
            V::Str(a) => V::Str(a.to_uppercase()),
            _ => V::Null,
        },
        E::Concat(a, b) => match (eval_env(a, row, env), eval_env(b, row, env)) {
            (V::Str(a), V::Str(b)) => V::Str(a + &b),
            _ => V::Null,
        },
        E::Cmp(op, a, b) => {
            let (a, b) = (eval_env(a, row, env), eval_env(b, row, env));
            from_truth(match *op {
                "=" => equals(&a, &b),
                "<>" => equals(&a, &b).map(|equal| !equal),
                _ => compare(&a, &b).map(|ordering| match ordering {
                    None => false,
                    Some(ordering) => match *op {
                        "<" => ordering == Ordering::Less,
                        "<=" => ordering != Ordering::Greater,
                        ">" => ordering == Ordering::Greater,
                        _ => ordering != Ordering::Less,
                    },
                }),
            })
        }
        E::Logic(op, a, b) => {
            let (a, b) = (truth(&eval_env(a, row, env)), truth(&eval_env(b, row, env)));
            from_truth(match *op {
                "AND" => match (a, b) {
                    (Some(false), _) | (_, Some(false)) => Some(false),
                    (Some(true), Some(true)) => Some(true),
                    _ => None,
                },
                "OR" => match (a, b) {
                    (Some(true), _) | (_, Some(true)) => Some(true),
                    (Some(false), Some(false)) => Some(false),
                    _ => None,
                },
                _ => match (a, b) {
                    (Some(a), Some(b)) => Some(a != b),
                    _ => None,
                },
            })
        }
        E::Not(a) => from_truth(truth(&eval_env(a, row, env)).map(|value| !value)),
        E::IsNull(a, null) => V::Bool(eval_env(a, row, env).is_null() == *null),
        E::In(a, items) => {
            let a = eval_env(a, row, env);
            if items.is_empty() {
                return V::Bool(false);
            }
            let mut unknown = false;
            for item in items {
                match equals(&a, item) {
                    Some(true) => return V::Bool(true),
                    None => unknown = true,
                    Some(false) => {}
                }
            }
            if unknown { V::Null } else { V::Bool(false) }
        }
        E::Text(op, a, b) => match (eval_env(a, row, env), eval_env(b, row, env)) {
            (V::Str(a), V::Str(b)) => V::Bool(match *op {
                "STARTS WITH" => a.starts_with(&b),
                "ENDS WITH" => a.ends_with(&b),
                _ => a.contains(&b),
            }),
            _ => V::Null,
        },
        E::Case(c, a, b) => {
            if truth(&eval_env(c, row, env)) == Some(true) {
                eval_env(a, row, env)
            } else {
                eval_env(b, row, env)
            }
        }
        E::Coalesce(a, b) => {
            let a = eval_env(a, row, env);
            if a.is_null() {
                eval_env(b, row, env)
            } else {
                a
            }
        }
        E::Var(index) => env[*index].clone(),
        E::List(items) => V::List(items.iter().map(|item| eval_env(item, row, env)).collect()),
        E::Range(a, b) => V::List((*a..=*b).map(V::Int).collect()),
        E::Index(l, i) => match (eval_env(l, row, env), eval_env(i, row, env)) {
            (V::List(items), V::Int(i)) => {
                let len = items.len() as i64;
                let at = if i < 0 { i + len } else { i };
                if (0..len).contains(&at) {
                    items[at as usize].clone()
                } else {
                    V::Null
                }
            }
            _ => V::Null,
        },
        E::Slice(l, a, b) => match eval_env(l, row, env) {
            V::List(items) => {
                let len = items.len() as i64;
                let clamp = |i: i64| (if i < 0 { i + len } else { i }).clamp(0, len) as usize;
                let (a, b) = (clamp(*a), clamp(*b));
                V::List(if a < b { items[a..b].to_vec() } else { vec![] })
            }
            _ => V::Null,
        },
        E::ListConcat(a, b) => match (eval_env(a, row, env), eval_env(b, row, env)) {
            (V::List(mut a), V::List(b)) => {
                a.extend(b);
                V::List(a)
            }
            _ => V::Null,
        },
        E::Tail(l) => match eval_env(l, row, env) {
            V::List(items) => V::List(items.into_iter().skip(1).collect()),
            _ => V::Null,
        },
        E::SizeList(l) => match eval_env(l, row, env) {
            V::List(items) => V::Int(items.len() as i64),
            _ => V::Null,
        },
        E::Comprehension(var, l, filter, projection) => match eval_env(l, row, env) {
            V::List(items) => {
                let mut out = Vec::new();
                for item in items {
                    env.truncate(*var);
                    env.push(item);
                    let keep = filter
                        .as_ref()
                        .is_none_or(|filter| truth(&eval_env(filter, row, env)) == Some(true));
                    if keep {
                        out.push(eval_env(projection, row, env));
                    }
                }
                env.truncate(*var);
                V::List(out)
            }
            _ => V::Null,
        },
        E::Quantifier(kind, var, l, p) => match eval_env(l, row, env) {
            V::List(items) => {
                let (mut yes, mut no, mut unknown) = (0, 0, 0);
                for item in items {
                    env.truncate(*var);
                    env.push(item);
                    match truth(&eval_env(p, row, env)) {
                        Some(true) => yes += 1,
                        Some(false) => no += 1,
                        None => unknown += 1,
                    }
                }
                env.truncate(*var);
                let _ = no;
                from_truth(match *kind {
                    "any" => {
                        if yes > 0 {
                            Some(true)
                        } else if unknown > 0 {
                            None
                        } else {
                            Some(false)
                        }
                    }
                    "none" => {
                        if yes > 0 {
                            Some(false)
                        } else if unknown > 0 {
                            None
                        } else {
                            Some(true)
                        }
                    }
                    "all" => {
                        if no > 0 {
                            Some(false)
                        } else if unknown > 0 {
                            None
                        } else {
                            Some(true)
                        }
                    }
                    _ => {
                        if yes > 1 {
                            Some(false)
                        } else if unknown > 0 {
                            None
                        } else {
                            Some(yes == 1)
                        }
                    }
                })
            }
            _ => V::Null,
        },
        E::Lower(a) => match eval_env(a, row, env) {
            V::Str(a) => V::Str(a.to_lowercase()),
            _ => V::Null,
        },
        E::Reverse(a) => match eval_env(a, row, env) {
            V::Str(a) => V::Str(a.chars().rev().collect()),
            _ => V::Null,
        },
        E::Substring(a, start, length) => match eval_env(a, row, env) {
            V::Str(a) => V::Str(
                a.chars()
                    .skip(*start as usize)
                    .take(*length as usize)
                    .collect(),
            ),
            _ => V::Null,
        },
    }
}
