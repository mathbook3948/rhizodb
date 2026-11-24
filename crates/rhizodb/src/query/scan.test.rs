use std::cell::Cell as Flag;
use std::fmt::Write as _;

use proptest::prelude::*;

use crate::Database;
use crate::ErrorKind;
use crate::Parameters;
use crate::QueryResult;
use crate::Value;
use crate::error::Error;
use crate::test_scale;

thread_local! {
    static ROW_PATH: Flag<bool> = const { Flag::new(false) };
    static SCANS: Flag<usize> = const { Flag::new(0) };
}

pub(crate) fn row_at_a_time() -> bool {
    ROW_PATH.with(Flag::get)
}

pub(crate) fn scanned() {
    SCANS.with(|scans| scans.set(scans.get() + 1));
}

fn scans() -> usize {
    SCANS.with(Flag::get)
}

type Outcome = Result<Vec<String>, ErrorKind>;

fn outcome(result: Result<QueryResult, Error>) -> Outcome {
    result
        .map(|result| result.rows().iter().map(|row| format!("{row:?}")).collect())
        .map_err(|error| error.kind())
}

struct Compared {
    fast: Outcome,
    row: Outcome,
    scanned: bool,
}

fn compare(mut run: impl FnMut() -> Result<QueryResult, Error>) -> Compared {
    let before = scans();
    let fast = outcome(run());
    let scanned = scans() > before;
    ROW_PATH.with(|flag| flag.set(true));
    let row = outcome(run());
    ROW_PATH.with(|flag| flag.set(false));
    Compared { fast, row, scanned }
}

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

    fn chance(&mut self, numerator: u64, denominator: u64) -> bool {
        self.below(denominator) < numerator
    }

    fn pick<'a, V>(&mut self, items: &'a [V]) -> &'a V {
        &items[self.below(items.len() as u64) as usize]
    }
}

const STRINGS: [&str; 7] = ["", "a", "ab", "b", "ba", "abc", "x"];
const FLOATS: [f64; 8] = [0.0, -0.0, 1.0, 2.5, f64::NAN, f64::INFINITY, -1.5, 3.0];

fn scalar(rng: &mut Rng) -> Value {
    match rng.below(10) {
        0..=3 => Value::Integer(rng.below(6) as i64 - 2),
        4 | 5 => Value::Float(*rng.pick(&FLOATS)),
        6 | 7 => Value::String((*rng.pick(&STRINGS)).to_owned()),
        8 => Value::Boolean(rng.chance(1, 2)),
        _ => Value::Null,
    }
}

fn any_value(rng: &mut Rng) -> Value {
    match rng.below(12) {
        0 => Value::List(vec![Value::Integer(1), Value::Integer(rng.below(3) as i64)]),
        1 => Value::List(vec![Value::String("a".to_owned())]),
        2 => Value::Bytes(vec![rng.below(3) as u8]),
        _ => scalar(rng),
    }
}

fn typed(rng: &mut Rng, kind: u64) -> Value {
    if rng.chance(1, 12) {
        return any_value(rng);
    }
    match kind {
        0 => Value::Integer(rng.below(9) as i64 - 3),
        1 => Value::String((*rng.pick(&STRINGS)).to_owned()),
        _ => any_value(rng),
    }
}

fn properties(rng: &mut Rng, key: i64) -> Value {
    let mut map = std::collections::BTreeMap::new();
    map.insert("k".to_owned(), Value::Integer(key));
    for (name, kind) in [("a", 0), ("b", 1), ("c", 2)] {
        if !rng.chance(1, 5) {
            map.insert(name.to_owned(), typed(rng, kind));
        }
    }
    Value::Map(map)
}

fn parameters(entries: Vec<(String, Value)>) -> Parameters {
    entries.into_iter().collect()
}

struct Graph {
    db: Database,
    keys: i64,
    log: String,
}

impl Graph {
    fn write(&mut self, query: &str, parameters: &Parameters) {
        let _ = writeln!(self.log, "{query} {parameters:?}");
        let mut tx = self.db.begin_write().unwrap();
        tx.execute(query, parameters).unwrap();
        tx.commit().unwrap();
    }

    fn build(nodes: i64, rng: &mut Rng) -> Self {
        let mut graph = Graph {
            db: Database::in_memory().unwrap(),
            keys: nodes,
            log: String::new(),
        };
        let rows = (0..nodes).map(|key| properties(rng, key)).collect();
        graph.write(
            "UNWIND $rows AS r CREATE (n) SET n = r",
            &parameters(vec![("rows".to_owned(), Value::List(rows))]),
        );
        for label in ["A", "B"] {
            let keys = (0..nodes)
                .filter(|_| rng.chance(2, 3))
                .map(Value::Integer)
                .collect();
            graph.write(
                &format!("MATCH (n) WHERE n.k IN $keys SET n:{label}"),
                &parameters(vec![("keys".to_owned(), Value::List(keys))]),
            );
        }
        let deleted = (0..nodes)
            .filter(|_| rng.chance(1, 10))
            .map(Value::Integer)
            .collect();
        graph.write(
            "MATCH (n) WHERE n.k IN $keys DETACH DELETE n",
            &parameters(vec![("keys".to_owned(), Value::List(deleted))]),
        );
        for _ in 0..rng.below(6) {
            let edit = edit(rng, graph.keys);
            graph.keys += 1;
            graph.write(&edit.0, &edit.1);
        }
        graph
    }
}

fn edit(rng: &mut Rng, next_key: i64) -> (String, Parameters) {
    let key = Value::Integer(rng.below(next_key.max(1) as u64) as i64);
    let property = *rng.pick(&["a", "b", "c"]);
    match rng.below(7) {
        0 => (
            format!("MATCH (n) WHERE n.k = $key REMOVE n.{property}"),
            parameters(vec![("key".to_owned(), key)]),
        ),
        1 => (
            "MATCH (n) WHERE n.k = $key DETACH DELETE n".to_owned(),
            parameters(vec![("key".to_owned(), key)]),
        ),
        2 => (
            "CREATE (n:A) SET n = $p".to_owned(),
            parameters(vec![("p".to_owned(), properties(rng, next_key))]),
        ),
        3 => (
            format!("MATCH (n) WHERE n.k = $key SET n:{}", rng.pick(&["A", "B"])),
            parameters(vec![("key".to_owned(), key)]),
        ),
        4 => (
            format!(
                "MATCH (n) WHERE n.k = $key SET n.{property} = date('2020-01-0{}')",
                rng.below(3) + 1
            ),
            parameters(vec![("key".to_owned(), key)]),
        ),
        _ => (
            format!("MATCH (n) WHERE n.k = $key SET n.{property} = $v"),
            parameters(vec![
                ("key".to_owned(), key),
                ("v".to_owned(), any_value(rng)),
            ]),
        ),
    }
}

struct Query {
    text: String,
    parameters: Vec<(String, Value)>,
}

impl Query {
    fn param(&mut self, value: Value) -> String {
        let name = format!("p{}", self.parameters.len());
        self.parameters.push((name.clone(), value));
        format!("${name}")
    }

    fn operand(&mut self, rng: &mut Rng) -> String {
        match rng.below(8) {
            0..=3 => format!("n.{}", rng.pick(&["a", "b", "c", "k", "missing"])),
            4 => format!("{}", rng.below(5) as i64 - 1),
            5 => "n".to_owned(),
            _ => {
                let value = any_value(rng);
                self.param(value)
            }
        }
    }

    fn pred(&mut self, rng: &mut Rng, depth: u32) -> String {
        let branch = if depth == 0 {
            rng.below(7)
        } else {
            rng.below(12)
        };
        match branch {
            0 | 1 => {
                let ops = ["=", "<>", "<", "<=", ">", ">="];
                let mut text = self.operand(rng);
                for _ in 0..=rng.below(2) {
                    let _ = write!(text, " {} {}", rng.pick(&ops), self.operand(rng));
                }
                text
            }
            2 => format!(
                "{} IS {}NULL",
                self.operand(rng),
                if rng.chance(1, 2) { "NOT " } else { "" }
            ),
            3 => {
                let op = *rng.pick(&["STARTS WITH", "ENDS WITH", "CONTAINS"]);
                let part = match rng.below(3) {
                    0 => self.operand(rng),
                    _ => {
                        let text = Value::String((*rng.pick(&STRINGS)).to_owned());
                        self.param(text)
                    }
                };
                format!("{} {op} {part}", self.operand(rng))
            }
            4 => {
                let list = match rng.below(5) {
                    0 => Value::Null,
                    _ => Value::List((0..rng.below(4)).map(|_| any_value(rng)).collect()),
                };
                let list = self.param(list);
                format!("{} IN {list}", self.operand(rng))
            }
            5 if rng.chance(1, 3) => (*rng.pick(&["true", "false", "null"])).to_owned(),
            5 => {
                let ops = ["=", "<>", "<", "<=", ">", ">="];
                let text = Value::String((*rng.pick(&STRINGS)).to_owned());
                let text = self.param(text);
                let property = format!("n.{}", rng.pick(&["b", "c"]));
                if rng.chance(1, 2) {
                    format!("{text} {} {property}", rng.pick(&ops))
                } else {
                    format!("{property} {} {text}", rng.pick(&ops))
                }
            }
            6 => match rng.below(4) {
                0 => "n.a + 1 > 1".to_owned(),
                1 => "n:A".to_owned(),
                2 => {
                    let flag = any_value(rng);
                    self.param(flag)
                }
                _ => "n.c".to_owned(),
            },
            7 => format!("NOT ({})", self.pred(rng, depth - 1)),
            _ => {
                let op = *rng.pick(&["AND", "OR", "XOR"]);
                format!(
                    "({}) {op} ({})",
                    self.pred(rng, depth - 1),
                    self.pred(rng, depth - 1)
                )
            }
        }
    }

    fn generate(rng: &mut Rng) -> Query {
        let mut query = Query {
            text: String::new(),
            parameters: Vec::new(),
        };
        let mut label = (*rng.pick(&["", "", ":A", ":B", ":Missing", ":A|B"])).to_owned();
        if rng.chance(1, 4) {
            let entries: Vec<String> = (0..1 + rng.below(2))
                .map(|_| {
                    let key = *rng.pick(&["a", "b", "k", "missing"]);
                    let value = query.operand(rng);
                    format!("{key}: {value}")
                })
                .collect();
            label = format!("{label} {{{}}}", entries.join(", "));
        }
        let filter = match rng.below(4) {
            0 => String::new(),
            _ => format!(" WHERE {}", query.pred(rng, 2)),
        };
        let skip_limit = |query: &mut Query, rng: &mut Rng| -> String {
            let mut text = String::new();
            if rng.chance(1, 3) {
                let skip = Value::Integer(rng.below(4) as i64);
                let _ = write!(text, " SKIP {}", query.param(skip));
            }
            if rng.chance(2, 3) {
                let limit = if rng.chance(1, 20) {
                    Value::Integer(-1)
                } else {
                    Value::Integer(rng.below(6) as i64)
                };
                let _ = write!(text, " LIMIT {}", query.param(limit));
            }
            text
        };
        let order = |rng: &mut Rng| -> String {
            let keys: Vec<String> = (0..=rng.below(2))
                .map(|_| {
                    format!(
                        "n.{}{}",
                        rng.pick(&["a", "b", "c", "k"]),
                        if rng.chance(1, 2) { " DESC" } else { "" }
                    )
                })
                .collect();
            keys.join(", ")
        };
        let mut head = format!("MATCH (n{label}){filter}");
        if rng.chance(1, 8) {
            let xs = Value::List((0..rng.below(3)).map(|_| scalar(rng)).collect());
            let xs = query.param(xs);
            head = format!("UNWIND {xs} AS x MATCH (n{label}) WHERE n.a > x OR n.k = x");
        }
        let tail = match rng.below(19) {
            0 => "RETURN count(*)".to_owned(),
            1 => "RETURN count(n), count(n.a), count(n.missing)".to_owned(),
            2 => {
                "RETURN count(n.a), sum(n.k), min(n.a), max(n.b), avg(n.k), collect(n.c)".to_owned()
            }
            3 => "RETURN n.c AS c, count(*) AS count, collect(n.k) AS keys".to_owned(),
            4 => "RETURN n.a, count(DISTINCT n.b), min(n.c)".to_owned(),
            5 => "RETURN sum(n.a), avg(n.c)".to_owned(),
            6 => {
                let percentile = Value::Float(rng.below(3) as f64 / 2.0);
                format!("RETURN percentileCont(n.k, {})", query.param(percentile))
            }
            7 => format!(
                "RETURN n.k, n.a ORDER BY {}{}",
                order(rng),
                skip_limit(&mut query, rng)
            ),
            8 => format!(
                "RETURN n ORDER BY {}{}",
                order(rng),
                skip_limit(&mut query, rng)
            ),
            9 => format!("RETURN DISTINCT n.a{}", skip_limit(&mut query, rng)),
            10 => format!(
                "RETURN DISTINCT n.a AS a, n.b AS b ORDER BY a DESC, b{}",
                skip_limit(&mut query, rng)
            ),
            11 => format!("RETURN n.k, n.c{}", skip_limit(&mut query, rng)),
            12 => format!(
                "WITH n.a AS a, n.k AS k WHERE a > 0 OR k < 3 RETURN a, k ORDER BY k{}",
                skip_limit(&mut query, rng)
            ),
            13 => "UNWIND [1, 2] AS y RETURN n.k, y ORDER BY n.k, y".to_owned(),
            14 => format!(
                "RETURN n.k AS k ORDER BY k DESC{}",
                skip_limit(&mut query, rng)
            ),
            15 => "WITH n RETURN n.k ORDER BY n.k".to_owned(),
            16 => format!(
                "RETURN n.k + 1 AS k ORDER BY {}{}",
                order(rng),
                skip_limit(&mut query, rng)
            ),
            17 => "RETURN n.a AS a, n.k AS k ORDER BY a, k LIMIT 3".to_owned(),
            _ => format!(
                "RETURN DISTINCT n.c ORDER BY n.c{}",
                skip_limit(&mut query, rng)
            ),
        };
        query.text = format!("{head} {tail}");
        query
    }
}

struct Report {
    queries: usize,
    scanned: usize,
}

fn check(graph: &mut Graph, rng: &mut Rng, queries: usize) -> Result<Report, String> {
    let mut report = Report {
        queries: 0,
        scanned: 0,
    };
    let generated: Vec<Query> = (0..queries).map(|_| Query::generate(rng)).collect();
    let mut verify = |query: &Query, compared: Compared, place: &str, log: &str| {
        report.queries += 1;
        report.scanned += usize::from(compared.scanned);
        if compared.fast != compared.row {
            return Err(format!(
                "{place}: {} {:?}\nfast: {:?}\nrow: {:?}\nstatements:\n{log}",
                query.text, query.parameters, compared.fast, compared.row
            ));
        }
        Ok(())
    };
    for query in &generated {
        let parameters = parameters(query.parameters.clone());
        let read = graph.db.begin_read().unwrap();
        let compared = compare(|| read.execute(&query.text, &parameters));
        verify(query, compared, "committed", &graph.log)?;
    }
    let mut log = graph.log.clone();
    let mut edits = Vec::new();
    for _ in 0..1 + rng.below(8) {
        let (text, edit_parameters) = edit(rng, graph.keys);
        graph.keys += 1;
        let _ = writeln!(log, "uncommitted: {text} {edit_parameters:?}");
        edits.push((text, edit_parameters));
    }
    for query in &generated {
        let parameters = parameters(query.parameters.clone());
        let compared = compare(|| {
            let mut tx = graph.db.begin_write().unwrap();
            for (text, edit_parameters) in &edits {
                let _ = tx.execute(text, edit_parameters);
            }
            let result = tx.execute(&query.text, &parameters);
            if result.is_ok() {
                tx.rollback().unwrap();
            }
            result
        });
        verify(query, compared, "uncommitted", &log)?;
    }
    Ok(report)
}

fn equivalent(nodes: i64, seed: u64, queries: usize) -> Result<Report, String> {
    let mut rng = Rng(seed);
    let mut graph = Graph::build(nodes, &mut rng);
    check(&mut graph, &mut rng, queries)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(test_scale::cases(48)))]

    #[test]
    fn small_graphs_answer_as_the_row_path(nodes in 0i64..80, seed in any::<u64>()) {
        let report = equivalent(nodes, seed, 40).map_err(TestCaseError::fail)?;
        prop_assert!(report.queries == 80);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(test_scale::cases(3)))]

    #[test]
    fn graphs_across_row_group_boundaries_answer_as_the_row_path(
        nodes in prop::sample::select(vec![4095i64, 4096, 4097, 8193]),
        seed in any::<u64>(),
    ) {
        equivalent(nodes, seed, 60).map_err(TestCaseError::fail)?;
    }
}

#[test]
fn most_generated_queries_take_the_scan() {
    let mut queries = 0;
    let mut scanned = 0;
    for seed in 0..20 {
        let report = equivalent(30, seed, 40).unwrap();
        queries += report.queries;
        scanned += report.scanned;
    }
    assert!(
        scanned * 3 > queries,
        "{scanned} of {queries} queries scanned"
    );
}

fn db_with(query: &str) -> Database {
    let db = Database::in_memory().unwrap();
    db.execute(query, &Parameters::new()).unwrap();
    db
}

fn takes_scan(db: &Database, query: &str) -> Compared {
    let compared = compare(|| db.execute(query, &Parameters::new()));
    assert_eq!(compared.fast, compared.row, "{query}");
    compared
}

const PEOPLE: &str = "UNWIND range(0, 9999) AS i CREATE (:Person {id: i, age: CASE WHEN i % 7 = 0 THEN null ELSE i % 90 END, name: 'n' + toString(i % 300), bio: CASE WHEN i % 11 = 0 THEN 'likes rust' ELSE 'likes go' END})";

#[test]
fn each_benchmark_shape_takes_the_scan() {
    let db = db_with(PEOPLE);
    for query in [
        "MATCH (p:Person) WHERE p.age > 30 RETURN count(*)",
        "MATCH (p:Person) WHERE p.age IS NOT NULL RETURN p.id, p.age ORDER BY p.age DESC, p.id ASC LIMIT 10",
        "MATCH (p:Person) RETURN DISTINCT p.name",
        "MATCH (p:Person) WHERE p.bio CONTAINS 'rust' RETURN count(*)",
        "MATCH (p:Person) RETURN p.name, count(*), avg(p.age)",
        "MATCH (p:Person) WHERE p.age < 3 RETURN p ORDER BY p.id LIMIT 3",
        "MATCH (p) WHERE p.id >= 9990 UNWIND [1] AS x RETURN p.id",
        "MATCH (p:Person) RETURN toUpper(p.name) ORDER BY p.id LIMIT 2",
        "MATCH (p:Person {id: 3}) RETURN p.name",
        "MATCH (p:Person {id: 3, name: 'n3', id: 4}) RETURN p.name",
    ] {
        assert!(takes_scan(&db, query).scanned, "{query} did not scan");
    }
}

#[test]
fn shapes_the_scan_cannot_answer_take_the_row_path() {
    let db = db_with(PEOPLE);
    for query in [
        "MATCH (p:Person) WHERE p.age + 1 > 30 RETURN count(*)",
        "OPTIONAL MATCH (p:Person) WHERE p.age > 80 RETURN count(p)",
        "MATCH (p:Person {id: 3 + p.id}) RETURN p.name",
        "MATCH (p:Person|Other) WHERE p.age > 80 RETURN count(*)",
        "MATCH (p:Person) WHERE id(p) = 3 RETURN p.name",
        "MATCH (p:Person) WHERE p.name =~ 'n1.*' RETURN count(*)",
    ] {
        assert!(!takes_scan(&db, query).scanned, "{query} scanned");
    }
}

#[test]
fn a_property_index_is_read_before_any_scan() {
    let db = db_with(PEOPLE);
    db.create_property_index("age", crate::IndexTarget::Label("Person"), "age")
        .unwrap();
    let compared = takes_scan(&db, "MATCH (p:Person) WHERE p.age = 30 RETURN count(*)");
    assert!(!compared.scanned);
    assert!(takes_scan(&db, "MATCH (p:Person) WHERE p.id = 30 RETURN count(*)").scanned);
}

#[test]
fn a_type_error_in_an_aggregate_fails_on_the_scan_as_on_the_row_path() {
    let db = db_with("UNWIND [1, 2, 'three', 4] AS v CREATE (:N {v: v})");
    let compared = takes_scan(&db, "MATCH (n:N) RETURN sum(n.v)");
    assert!(compared.scanned);
    assert_eq!(compared.fast, Err(ErrorKind::TypeMismatch));
    let compared = takes_scan(&db, "MATCH (n:N) WHERE n.v < 3 RETURN sum(n.v)");
    assert_eq!(compared.fast.unwrap(), vec!["[Integer(3)]".to_owned()]);
}
