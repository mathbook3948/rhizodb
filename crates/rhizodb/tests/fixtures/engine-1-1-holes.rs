//! Writes the engine 1.1 fixture with dead blocks inside live extents, through the public API of
//! the last commit whose catalog recorded only a count of each extent's live blocks.
//! `generate-engine-1-1-holes.sh` builds it against that commit. Run as `engine-1-1-holes <path>`,
//! it writes the database over several opens, so later checkpoints leave blocks of earlier ones
//! dead, closes it cleanly, and prints each query the test checks, each followed by its answer on
//! an indented line, which the script keeps in `engine-1-1-holes.answers`.

use rhizodb::Database;
use rhizodb::IndexTarget;
use rhizodb::Parameters;

const QUERIES: &[&str] = &[
    "MATCH (n) RETURN count(*), sum(n.i)",
    "MATCH (n:Item) WHERE n.i % 10 = 0 RETURN count(*), sum(size(n.text)), min(n.text STARTS WITH 'long')",
    "MATCH (n:Item {i: 2990}) RETURN n.i, size(n.text), substring(n.text, 0, 12)",
    "MATCH ()-[r:NEXT]->() RETURN count(*), sum(r.w), min(r.w), max(r.w)",
    "MATCH (n:Item) WHERE n.i % 7 = 3 RETURN count(*), sum(n.age)",
];

fn exec(path: &str, queries: &[&str]) {
    let db = Database::open(path).unwrap();
    for query in queries {
        db.execute(query, &Parameters::new()).unwrap();
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        panic!("usage: engine-1-1-holes <path>");
    };
    {
        let db = Database::open(&path).unwrap();
        db.create_property_index("item i", IndexTarget::Label("Item"), "i")
            .unwrap();
    }
    exec(
        &path,
        &[
            "UNWIND range(0, 5999) AS i CREATE (:Item {i: i, age: i % 50})",
            "MATCH (a:Item), (b:Item) WHERE b.i = a.i + 1 AND a.i % 3 = 0 CREATE (a)-[:NEXT {w: a.i - 1000}]->(b)",
        ],
    );
    exec(
        &path,
        &[
            "MATCH (n:Item) WHERE n.i % 10 = 0 SET n.text = 'long ' + reduce(s = '', k IN range(1, 300) | s + toString(n.i + k)) + toString(n.i)",
            "MATCH (x:Item) WHERE x.i >= 3000 DETACH DELETE x",
        ],
    );
    exec(
        &path,
        &[
            "MATCH (n:Item) WHERE n.i % 7 = 3 SET n.age = n.i * 2",
            "MATCH ()-[r:NEXT]->() WHERE r.w % 11 = 5 DELETE r",
        ],
    );
    let db = Database::open(&path).unwrap();
    for query in QUERIES {
        let result = db.execute(query, &Parameters::new()).unwrap();
        println!("{query}\n  {:?}", result.rows());
    }
}
