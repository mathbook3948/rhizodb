//! Writes the format 14 fixture through the public API of the last commit that wrote format 14.
//! `generate-format-14.sh` builds it against that commit. Run as `format-14 <path>`, it writes
//! the database, compacts it, closes it, and prints each query the upgrade test checks, each
//! followed by its answer on an indented line, which the script keeps in `format-14.answers`.
//! Its nodes hold lists of floats that bfloat16 holds bit for bit, that only single precision
//! holds, and that need double precision, as format 14 stored them, some changed after they
//! were written, and a vector index over one of them.

use rhizodb::Database;
use rhizodb::IndexTarget;
use rhizodb::VectorMetric;
use rhizodb::Parameters;
use rhizodb::Value;

const QUERIES: &[&str] = &[
    "MATCH (v:Vec) RETURN count(*), min(v.i), max(v.i)",
    "MATCH (v:Vec) WHERE v.i % 50 = 7 RETURN v.i, v.brain, v.single, v.double ORDER BY v.i",
    "MATCH (v:Vec) RETURN sum(v.brain[3]), sum(v.single[5]), sum(v.double[1]), sum(size(v.brain))",
    "MATCH (v:Vec) WHERE v.brain[0] > 0.5 RETURN count(*)",
    "MATCH (v:Vec) WHERE v.i % 9 = 0 RETURN count(v.brain), count(v.single)",
    "CALL db.index.vector.queryNodes('vec brain', [0.5, -0.25, 0.125, 1.0], 5) YIELD node, score RETURN node.i, score",
];

fn run(db: &Database, query: &str) {
    let result = db.execute(query, &Parameters::new()).unwrap();
    println!("{query}\n  {:?}", result.rows());
}

fn floats(values: impl Iterator<Item = f64>) -> Value {
    Value::List(values.map(Value::Float).collect())
}

fn build(db: &Database) {
    let rows: Vec<Value> = (0..600i64)
        .map(|i| {
            let brain = floats((0..4).map(|at| f64::from(((i * 7 + at * 13) % 129 - 64) as i32) / 64.0));
            let single = floats((0..8).map(|at| f64::from((i as f32 + at as f32) / 3.0)));
            let double = floats((0..3).map(|at| (i as f64 + at as f64) / 7.0));
            Value::Map(
                [
                    ("i".to_owned(), Value::Integer(i)),
                    ("brain".to_owned(), brain),
                    ("single".to_owned(), single),
                    ("double".to_owned(), double),
                ]
                .into_iter()
                .collect(),
            )
        })
        .collect();
    db.execute(
        "UNWIND $rows AS r CREATE (:Vec {i: r.i, brain: r.brain, single: r.single, double: r.double})",
        &Parameters::new().with("rows", Value::List(rows)),
    )
    .unwrap();
    db.create_vector_index("vec brain", IndexTarget::Label("Vec"), "brain", 4, VectorMetric::Cosine)
        .unwrap();
    db.execute(
        "MATCH (v:Vec) WHERE v.i % 9 = 0 REMOVE v.single SET v.brain = [0.75, -0.5, 0.25, 2.0]",
        &Parameters::new(),
    )
    .unwrap();
    db.execute("MATCH (v:Vec) WHERE v.i % 11 = 0 DELETE v", &Parameters::new())
        .unwrap();
    for query in QUERIES {
        run(db, query);
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        panic!("usage: format-14 <path>");
    };
    let db = Database::open(&path).unwrap();
    build(&db);
    db.compact().unwrap();
}
