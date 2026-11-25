//! Writes the engine 1.1 fixture through the public API of the last commit that gave each kind of
//! block an open extent of its own. `generate-engine-1-1.sh` builds it against that commit. Run as
//! `engine-1-1 <path>`, it writes the database, closes it cleanly, and prints each query the test
//! checks, each followed by its answer on an indented line, which the script keeps in
//! `engine-1-1.answers`. The file it leaves has an open extent for tree pages, one for long
//! values, and one for the catalog.

use rhizodb::Database;
use rhizodb::IndexTarget;
use rhizodb::Parameters;

const QUERIES: &[&str] = &[
    "MATCH (n) RETURN count(*), sum(n.i)",
    "MATCH (n:Item) WHERE n.i % 10 = 0 RETURN count(*), sum(size(n.text)), min(n.text STARTS WITH 'long'), max(n.text ENDS WITH '9')",
    "MATCH (n:Item {i: 2990}) RETURN n.i, size(n.text), substring(n.text, 0, 12)",
    "MATCH ()-[r:NEXT]->() RETURN count(*), sum(r.w), min(r.w), max(r.w)",
    "MATCH (a:Item)-[:NEXT]->(b:Item) WHERE a.i >= 1500 AND a.i < 1505 RETURN a.i, b.i ORDER BY a.i",
    "MATCH (n:Item) WHERE n.i % 7 = 3 RETURN count(*), sum(n.age)",
];

fn run(db: &Database, query: &str) {
    let result = db.execute(query, &Parameters::new()).unwrap();
    println!("{query}\n  {:?}", result.rows());
}

fn exec(db: &Database, query: &str) {
    db.execute(query, &Parameters::new()).unwrap();
}

fn build(db: &Database) {
    db.create_property_index("item i", IndexTarget::Label("Item"), "i")
        .unwrap();
    exec(db, "UNWIND range(0, 2999) AS i CREATE (:Item {i: i})");
    exec(
        db,
        "MATCH (n:Item) WHERE n.i % 10 = 0 SET n.text = 'long ' + reduce(s = '', k IN range(1, 300) | s + toString(n.i + k)) + toString(n.i)",
    );
    exec(
        db,
        "MATCH (a:Item), (b:Item) WHERE b.i = a.i + 1 AND a.i % 3 = 0 CREATE (a)-[:NEXT {w: a.i - 1000}]->(b)",
    );
    exec(db, "MATCH (n:Item) WHERE n.i % 7 = 3 SET n.age = n.i * 2");
    exec(db, "MATCH ()-[r:NEXT]->() WHERE r.w % 11 = 5 DELETE r");
    exec(db, "MATCH (x:Item) WHERE x.i % 1001 = 9 DETACH DELETE x");
    for query in QUERIES {
        run(db, query);
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        panic!("usage: engine-1-1 <path>");
    };
    build(&Database::open(&path).unwrap());
}
