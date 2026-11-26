//! Random read queries over random graphs, each run on one thread and split across four, which
//! must give the same rows in the same order, or fail with the same error.

mod support;

use rhizodb::Database;
use rhizodb::OpenOptions;
use rhizodb::Parameters;
use support::Rng;
use support::cases;
use support::parallel_options;
use support::regression_seeds;
use support::seed;

const FAST_GRAPHS: u64 = 24;
const QUERIES_PER_GRAPH: usize = 60;

const LABELS: [&str; 4] = ["", ":A", ":B", ":A&B"];

const FILTERS: [&str; 12] = [
    "",
    "WHERE n.x > 2",
    "WHERE n.x IS NOT NULL",
    "WHERE n.name CONTAINS 'a'",
    "WHERE n.name STARTS WITH 'b' OR n.f < 0.5",
    "WHERE n.x IN [1, 2, 3, 'c']",
    "WHERE n.x + 1 > 3",
    "WHERE EXISTS { (n)-[:R]->() }",
    "WHERE n.g = 'g1'",
    "WHERE NOT n.x = 4",
    "WHERE n.big > 0",
    "WHERE size(n.name) > 2",
];

const TAILS: [&str; 32] = [
    "RETURN count(*)",
    "RETURN count(n.x), sum(n.x), avg(n.x), min(n.x), max(n.x)",
    "RETURN sum(n.f), avg(n.f), stDev(n.f), stDevP(n.f)",
    "RETURN n.g AS g, count(*), collect(n.x) ORDER BY g",
    "RETURN n.g AS g, count(*), sum(n.f), collect(n.name)",
    "RETURN count(DISTINCT n.x), sum(DISTINCT n.x), avg(DISTINCT n.f), collect(DISTINCT n.g)",
    "RETURN percentileCont(n.f, 0.5), percentileDisc(n.x, 0.3), min(n.name), max(n.f)",
    "RETURN n.g AS g, min(n.x), max(DISTINCT n.x), min(CASE WHEN n.f > 0.5 THEN 1 ELSE 1.0 END), \
     max(CASE WHEN n.f > 0.5 THEN 1 ELSE 1.0 END), \
     max(DISTINCT CASE WHEN n.f > 0.5 THEN 1 ELSE 1.0 END) ORDER BY g",
    "RETURN n.g AS g, percentileCont(n.f, n.p) ORDER BY g",
    "RETURN sum(n.big), count(n.big)",
    "RETURN n.x, n.name ORDER BY n.x DESC LIMIT 5",
    "RETURN n.x, n.f ORDER BY n.f, n.x SKIP 2 LIMIT 4",
    "RETURN n.name SKIP 3 LIMIT 4",
    "RETURN n.x LIMIT 3",
    "RETURN DISTINCT n.g",
    "RETURN DISTINCT n.g, n.x ORDER BY n.x LIMIT 3",
    "RETURN n ORDER BY n.f DESC LIMIT 2",
    "RETURN n.x, n.name",
    "RETURN n.x + 1 AS y ORDER BY y LIMIT 6",
    "WITH n WHERE n.f > 0.3 RETURN n.name, n.x",
    "WITH n.g AS g, count(*) AS c WHERE c > 1 RETURN g, c",
    "UNWIND [1, 2] AS k RETURN n.x, k",
    "MATCH (n)-[r:R]->(m) RETURN m.g, count(*), avg(n.f), collect(type(r))",
    "MATCH (n)-[:R]->(m)-[:S]->(o) RETURN count(*), collect(o.x)",
    "MATCH (n)-[:R|S]-(m) RETURN n.x, m.x ORDER BY n.x, m.x LIMIT 7",
    "MATCH (m:B) WHERE m.x = n.x RETURN count(*), collect(m.name)",
    "RETURN n.x ORDER BY n.x LIMIT $limit",
    "CALL (n) { MATCH (n)-[:R]->(m) RETURN count(m) AS c } RETURN n.x, c ORDER BY n.x, c LIMIT 6",
    "CALL (n) { RETURN n.x * 2 AS d } RETURN d, count(*)",
    "WITH n WHERE VALUE { MATCH (n)-[:R]->(m) RETURN count(m) } > 1 RETURN n.name, n.x",
    "RETURN n.x AS x NEXT RETURN sum(x), count(*)",
    "RETURN count(*) AS c NEXT MATCH (m) WHERE m.x < c RETURN m.x, m.name LIMIT 4",
];

fn value(rng: &mut Rng) -> String {
    match rng.below(8) {
        0 => "null".to_owned(),
        1 => format!("{}.5", rng.below(6)),
        2 => "'c'".to_owned(),
        3 => format!("[{}]", rng.below(3)),
        _ => rng.below(7).to_string(),
    }
}

fn node(rng: &mut Rng) -> String {
    let labels = rng.pick(&[":A", ":B", ":A:B", ""]);
    let name = rng.pick(&["'ab'", "'ba'", "'cab'", "'b'", "null", "'abba'"]);
    let big = rng.pick(&["9223372036854775000", "-9223372036854775000", "1", "null"]);
    format!(
        "({labels} {{x: {}, f: {}, name: {name}, g: 'g{}', big: {big}, p: {}}})",
        value(rng),
        rng.below(1000) as f64 / 997.0,
        rng.below(3),
        rng.pick(&["0.5", "0.5", "0.5", "0.25"]),
    )
}

fn build(databases: &[&Database], rng: &mut Rng, nodes: u64) {
    let mut statements = Vec::new();
    for _ in 0..nodes {
        statements.push(format!("INSERT {}", node(rng)));
    }
    for _ in 0..nodes * 2 {
        statements.push(format!(
            "MATCH (a), (b) WHERE id(a) = {} AND id(b) = {} INSERT (a)-[:{}]->(b)",
            rng.below(nodes),
            rng.below(nodes),
            rng.pick(&["R", "S"]),
        ));
    }
    for db in databases {
        for statement in &statements {
            db.execute(statement, &Parameters::new()).unwrap();
        }
    }
}

fn query(rng: &mut Rng) -> String {
    let label = rng.pick(&LABELS);
    let tail = rng.pick(&TAILS);
    if tail.starts_with("MATCH (n)") {
        return tail.replacen("(n)", &format!("(n{label})"), 1);
    }
    format!("MATCH (n{label}) {} {tail}", rng.pick(&FILTERS))
}

fn outcome(db: &Database, query: &str, parameters: &Parameters) -> String {
    match db.execute(query, parameters) {
        Ok(result) => format!("{:?}", result.rows()),
        Err(error) => format!("error {:?}: {error}", error.kind()),
    }
}

fn compare(serial: &Database, parallel: &Database, rng: &mut Rng, seed: u64) {
    for _ in 0..QUERIES_PER_GRAPH {
        let query = query(rng);
        let parameters = Parameters::from([(
            "limit".to_owned(),
            rhizodb::Value::Integer(rng.below(5) as i64),
        )]);
        let alone = outcome(serial, &query, &parameters);
        let split = outcome(parallel, &query, &parameters);
        assert_eq!(
            alone, split,
            "seed {seed} (replay with RHIZODB_TEST_SEED={seed}): {query}"
        );
        let read = parallel.begin_read().unwrap();
        let in_transaction = match read.execute(&query, &parameters) {
            Ok(result) => format!("{:?}", result.rows()),
            Err(error) => format!("error {:?}: {error}", error.kind()),
        };
        assert_eq!(
            alone, in_transaction,
            "seed {seed}: {query} in a read transaction"
        );
    }
}

#[test]
#[ignore = "left out of mise run check, run by mise run random"]
fn random_reads_give_the_same_answer_on_one_thread_and_on_four() {
    let base = seed();
    let fresh = (0..cases(FAST_GRAPHS)).map(|offset| base.wrapping_add(offset));
    for seed in regression_seeds("random_reads_give_the_same_answer_on_one_thread_and_on_four")
        .into_iter()
        .chain(fresh)
    {
        let mut rng = Rng::new(seed);
        let serial = OpenOptions::new().threads(1).in_memory().unwrap();
        let parallel = parallel_options().in_memory().unwrap();
        let nodes = rng.below(120);
        build(&[&serial, &parallel], &mut rng, nodes);
        compare(&serial, &parallel, &mut rng, seed);
    }
}

#[test]
#[ignore = "left out of mise run check, run by mise run random"]
fn reads_over_several_row_groups_give_the_same_answer_on_one_thread_and_on_four() {
    let seed = seed();
    let mut rng = Rng::new(seed);
    let serial = OpenOptions::new().threads(1).in_memory().unwrap();
    let parallel = OpenOptions::new()
        .threads(4)
        .parallel_threshold(1000)
        .in_memory()
        .unwrap();
    let load = "UNWIND range(0, 9999) AS i INSERT (:A {x: i % 7, f: toFloat(i % 101) / 101.0, \
                name: CASE i % 3 WHEN 0 THEN 'ab' WHEN 1 THEN 'ba' ELSE 'c' END, g: 'g' + \
                toString(i % 3), big: i * 922337203685477, p: 0.5})";
    for db in [&serial, &parallel] {
        db.execute(load, &Parameters::new()).unwrap();
        db.execute(
            "MATCH (a:A), (b:A) WHERE id(b) = (id(a) * 7 + 3) % 10000 INSERT (a)-[:R]->(b)",
            &Parameters::new(),
        )
        .unwrap();
    }
    compare(&serial, &parallel, &mut rng, seed);
}
