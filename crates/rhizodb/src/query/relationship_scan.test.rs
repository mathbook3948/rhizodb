use std::cell::Cell;

use crate::Database;
use crate::ErrorKind;
use crate::OpenOptions;
use crate::Parameters;
use crate::QueryResult;
use crate::Value;
use crate::error::Error;
use crate::query::scan::on_row_path;
use crate::test_scale;

thread_local! {
    static FOLDS: Cell<usize> = const { Cell::new(0) };
}

pub(crate) fn folded() {
    FOLDS.with(|folds| folds.set(folds.get() + 1));
}

fn folds() -> usize {
    FOLDS.with(Cell::get)
}

type Outcome = Result<Vec<String>, ErrorKind>;

fn outcome(result: Result<QueryResult, Error>) -> Outcome {
    result
        .map(|result| result.rows().iter().map(|row| format!("{row:?}")).collect())
        .map_err(|error| error.kind())
}

fn agrees(query: &str, mut run: impl FnMut(&str) -> Outcome) -> bool {
    let before = folds();
    let fast = run(query);
    let took = folds() > before;
    let row = on_row_path(|| run(query));
    assert_eq!(fast, row, "{query}");
    took
}

const QUERIES: [&str; 15] = [
    "MATCH ()-[r:T]->() RETURN count(*), count(r), count(r.p), sum(r.p), avg(r.p), min(r.p), max(r.p)",
    "MATCH (:A)-[r:T]->(:B) RETURN count(*), sum(r.p), max(r.q)",
    "MATCH (a:A)<-[r]-(b) RETURN count(a), count(b), sum(r.p), min(r.p)",
    "MATCH (a)-[r]->(b:B) RETURN avg(r.p), count(DISTINCT r.p)",
    "MATCH ()-[r:Nope]->() RETURN count(*), sum(r.p)",
    "MATCH (:Nope)-[r]->() RETURN count(*), sum(r.p)",
    "MATCH ()-[r:T]->() RETURN sum(r.nope), max(r.nope), count(r.nope)",
    "MATCH (a)-[r:U]->(a) RETURN count(*), sum(r.p)",
    "MATCH ()-[r:T]->() RETURN sum(r.p) + 1 AS s, count(*) * 2 AS c",
    "MATCH ()-[r]->() RETURN count(*), sum(r.q), min(r.q), max(r.p)",
    "MATCH ()<-[r:U]-(:A) RETURN avg(r.q), count(r.p)",
    "MATCH ()-[r:T]->() RETURN collect(r.p)",
    "MATCH ()-[r:T]->() RETURN sum(DISTINCT r.p)",
    "MATCH ()-[r:T*1..2]->() RETURN count(*)",
    "MATCH ()-[r:T*2]->() RETURN count(*)",
];

struct Rng(u64);

impl Rng {
    fn below(&mut self, bound: u64) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        (z ^ (z >> 31)) % bound.max(1)
    }
}

fn value(rng: &mut Rng, mixed: bool) -> Value {
    if !mixed || rng.below(4) != 0 {
        return match rng.below(8) {
            0 => Value::Null,
            _ => Value::Integer(rng.below(100) as i64 - 50),
        };
    }
    match rng.below(7) {
        0 => Value::Integer(i64::MAX - rng.below(3) as i64),
        1 => Value::Integer(i64::MIN + rng.below(3) as i64),
        2 => Value::Float([0.5, -0.0, 1.0, f64::NAN][rng.below(4) as usize]),
        3 => Value::String(["", "a", "b"][rng.below(3) as usize].to_owned()),
        4 => Value::Boolean(rng.below(2) == 0),
        5 => Value::List(vec![Value::Integer(1)]),
        _ => Value::Integer(1 << 53),
    }
}

fn run(tx: &mut crate::WriteTransaction<'_>, query: &str, parameters: Parameters) {
    tx.execute(query, &parameters).unwrap();
}

fn populate(
    tx: &mut crate::WriteTransaction<'_>,
    rng: &mut Rng,
    nodes: u64,
    rels: u64,
    mixed: bool,
) {
    let labels = ["", ":A", ":B", ":A:B"];
    for key in 0..nodes {
        let label = labels[rng.below(4) as usize];
        run(
            tx,
            &format!("CREATE (n{label} {{k: $k}})"),
            Parameters::from([("k", key as i64)]),
        );
    }
    for key in 0..rels {
        let rel_type = ["T", "U"][rng.below(2) as usize];
        let parameters = Parameters::new()
            .with("a", rng.below(nodes) as i64)
            .with("b", rng.below(nodes) as i64)
            .with("k", key as i64)
            .with("p", value(rng, mixed))
            .with("q", value(rng, mixed));
        run(
            tx,
            &format!(
                "MATCH (a {{k: $a}}), (b {{k: $b}}) CREATE (a)-[:{rel_type} {{k: $k, p: $p, q: $q}}]->(b)"
            ),
            parameters,
        );
    }
    let deleted: Vec<Value> = (0..rels)
        .filter(|_| rng.below(5) == 0)
        .map(|key| Value::Integer(key as i64))
        .collect();
    run(
        tx,
        "MATCH ()-[r]->() WHERE r.k IN $keys DELETE r",
        Parameters::new().with("keys", Value::List(deleted)),
    );
}

fn database(threads: usize) -> Database {
    OpenOptions::new()
        .threads(threads)
        .parallel_threshold(2)
        .in_memory()
        .unwrap()
}

fn check_graph(seed: u64, mixed: bool, threads: usize) -> usize {
    let mut shape = Rng(seed);
    let (nodes, rels) = (1 + shape.below(40), shape.below(150));
    let db = database(threads);
    let mut took = 0;
    for query in QUERIES {
        took += usize::from(agrees(query, |query| {
            let mut tx = db.begin_write().unwrap();
            populate(&mut tx, &mut Rng(seed), nodes, rels, mixed);
            outcome(tx.execute(query, &Parameters::new()))
        }));
    }
    let mut tx = db.begin_write().unwrap();
    populate(&mut tx, &mut Rng(seed), nodes, rels, mixed);
    tx.commit().unwrap();
    for query in QUERIES {
        took += usize::from(agrees(query, |query| {
            outcome(db.execute(query, &Parameters::new()))
        }));
    }
    took
}

#[test]
fn random_graphs_fold_as_the_row_path_does() {
    let seed = test_scale::seed();
    let mut took = 0;
    for case in 0..u64::from(test_scale::cases(12)) {
        for mixed in [false, true] {
            for threads in [1, 4] {
                took += check_graph(seed.wrapping_add(case), mixed, threads);
            }
        }
    }
    assert!(took > 0, "no query folded relationship chunks");
}

#[test]
fn the_benchmark_shape_folds_relationship_chunks() {
    let db = database(4);
    db.execute(
        "UNWIND range(0, 999) AS i CREATE (:Person {id: i})",
        &Parameters::new(),
    )
    .unwrap();
    db.execute(
        "MATCH (a:Person), (b:Person) WHERE b.id = (a.id * 7 + 3) % 1000 CREATE (a)-[:KNOWS {since: 1990 + a.id % 35}]->(b)",
        &Parameters::new(),
    )
    .unwrap();
    for query in [
        "MATCH (:Person)-[k:KNOWS]->(:Person) RETURN count(*), sum(k.since)",
        "MATCH ()-[k:KNOWS]->() RETURN sum(k.since)",
        "MATCH ()-[k:KNOWS]->() RETURN min(k.since), max(k.since), avg(k.since)",
    ] {
        assert!(
            agrees(query, |query| outcome(
                db.execute(query, &Parameters::new())
            )),
            "{query} did not fold"
        );
    }
}

#[test]
fn values_whose_fold_depends_on_order_leave_it_to_the_row_path() {
    let db = database(1);
    for (values, query) in [
        ("[9223372036854775807, 1, -1]", "RETURN sum(r.p)"),
        ("[1, -1, 9223372036854775807]", "RETURN sum(r.p)"),
        ("[9007199254740993, 1]", "RETURN avg(r.p)"),
        ("[1, 1.0]", "RETURN min(r.p), max(r.p)"),
        ("[0.1, 0.2, 0.3]", "RETURN sum(r.p)"),
        ("[1, 'a', true]", "RETURN sum(r.p)"),
        ("[1, 'a']", "RETURN max(r.p)"),
    ] {
        db.execute("MATCH (n) DETACH DELETE n", &Parameters::new())
            .unwrap();
        db.execute(
            &format!("UNWIND {values} AS v CREATE ()-[:T {{p: v}}]->()"),
            &Parameters::new(),
        )
        .unwrap();
        let query = format!("MATCH ()-[r:T]->() {query}");
        assert!(
            !agrees(&query, |query| outcome(
                db.execute(query, &Parameters::new())
            )),
            "{query} over {values} folded"
        );
    }
}

#[test]
fn mixed_values_that_cannot_change_the_result_still_fold() {
    let db = database(1);
    db.execute(
        "UNWIND [3, null, 'a', 2.5, -4] AS v CREATE ()-[:T {p: v}]->()",
        &Parameters::new(),
    )
    .unwrap();
    assert!(agrees(
        "MATCH ()-[r:T]->() RETURN count(r.p), count(*)",
        |query| { outcome(db.execute(query, &Parameters::new())) }
    ));
}

#[test]
fn a_snapshot_folds_the_relationships_it_saw() {
    let db = database(4);
    db.execute(
        "UNWIND range(1, 100) AS i CREATE ()-[:T {p: i}]->()",
        &Parameters::new(),
    )
    .unwrap();
    let read = db.begin_read().unwrap();
    db.execute(
        "MATCH ()-[r:T]->() WHERE r.p > 50 DELETE r",
        &Parameters::new(),
    )
    .unwrap();
    db.execute(
        "UNWIND range(1, 10) AS i CREATE ()-[:T {p: 1000}]->()",
        &Parameters::new(),
    )
    .unwrap();
    db.execute("MATCH ()-[r:T]->() SET r.p = r.p + 1", &Parameters::new())
        .unwrap();
    let query = "MATCH ()-[r:T]->() RETURN count(*), sum(r.p), max(r.p)";
    assert!(agrees(query, |query| outcome(
        read.execute(query, &Parameters::new())
    )));
    assert_eq!(
        read.execute(query, &Parameters::new()).unwrap().rows(),
        [vec![
            Value::Integer(100),
            Value::Integer(5050),
            Value::Integer(100)
        ]]
    );
    assert_eq!(
        db.execute(query, &Parameters::new()).unwrap().rows(),
        [vec![
            Value::Integer(60),
            Value::Integer(11_335),
            Value::Integer(1001)
        ]]
    );
}

#[test]
fn a_write_transaction_folds_its_own_uncommitted_changes() {
    let db = database(1);
    db.execute(
        "UNWIND range(1, 100) AS i CREATE ()-[:T {p: i}]->()",
        &Parameters::new(),
    )
    .unwrap();
    let mut tx = db.begin_write().unwrap();
    tx.execute(
        "MATCH ()-[r:T]->() WHERE r.p % 2 = 0 DELETE r",
        &Parameters::new(),
    )
    .unwrap();
    tx.execute(
        "MATCH ()-[r:T]->() WHERE r.p < 10 SET r.p = null",
        &Parameters::new(),
    )
    .unwrap();
    tx.execute(
        "CREATE ()-[:T {p: 500}]->(), ()-[:U {p: 7}]->()",
        &Parameters::new(),
    )
    .unwrap();
    let query = "MATCH ()-[r:T]->() RETURN count(*), count(r.p), sum(r.p), min(r.p)";
    assert!(agrees(query, |query| outcome(
        tx.execute(query, &Parameters::new())
    )));
    assert_eq!(
        tx.execute(query, &Parameters::new()).unwrap().rows(),
        [vec![
            Value::Integer(51),
            Value::Integer(46),
            Value::Integer(2475 + 500),
            Value::Integer(11),
        ]]
    );
    tx.rollback().unwrap();
    assert_eq!(
        db.execute(query, &Parameters::new()).unwrap().rows(),
        [vec![
            Value::Integer(100),
            Value::Integer(100),
            Value::Integer(5050),
            Value::Integer(1),
        ]]
    );
}
