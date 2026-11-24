//! Writes the format 9 fixture through the public API of the last commit that wrote format 9.
//! `generate-format-9.sh` builds it against that commit. Run as `format-9 <path>`, it writes
//! the database, compacts it, and prints each query the upgrade test checks, each followed by
//! its answer on an indented line, which the script keeps in `format-9.answers`. The graph puts
//! relationships of several types across three row groups: a hub with more outgoing and
//! incoming relationships than a row group has slots, self loops, parallel relationships,
//! deleted relationships, and relationship properties under a property index.

use rhizodb::Database;
use rhizodb::IndexTarget;
use rhizodb::Parameters;

const QUERIES: &[&str] = &[
    "MATCH (n) RETURN count(*)",
    "MATCH ()-[r]->() RETURN count(*), min(id(r)), max(id(r)), sum(id(r))",
    "MATCH ()-[r]->() RETURN type(r) AS t, count(*) ORDER BY t",
    "MATCH (h:Hub)-[r:FAN]->(x) RETURN count(*), sum(x.i), min(x.i), max(x.i)",
    "MATCH (h:Hub)<-[r:BACK]-(x) RETURN count(*), sum(x.i), sum(r.w)",
    "MATCH (h:Hub)-[r]-(x) RETURN count(*), count(DISTINCT x)",
    "MATCH (h:Hub)-[r]->(x) WHERE x.i < 5 OR x.i > 8995 RETURN id(r), type(r), x.i",
    "MATCH (h:Hub)<-[r]-(x) WHERE x.i < 5 OR x.i > 8995 RETURN id(r), type(r), x.i",
    "MATCH (a:Item)-[r:NEXT]->(b:Item) WHERE a.i >= 4090 AND a.i < 4100 RETURN a.i, b.i, r.w ORDER BY a.i",
    "MATCH (a:Item)-[r:NEXT]->(b:Item) RETURN count(*), sum(r.w)",
    "MATCH (a:Item)-[:NEXT*1..3]->(b:Item {i: 8200}) RETURN a.i ORDER BY a.i",
    "MATCH (a)-[r:SELF]->(a) RETURN a.i, r.w ORDER BY a.i",
    "MATCH (a)-[r:SELF]-(b) RETURN count(*)",
    "MATCH (a:Item {i: 10})-[r]->(b) RETURN id(r), type(r), b.i",
    "MATCH (a:Item {i: 10})<-[r]-(b) RETURN id(r), type(r), b.i",
    "MATCH (a:Item {i: 10})-[r:PAR]-(b) RETURN id(r), r.k, b.i",
    "MATCH ()-[r:PAR {k: 2}]->() RETURN count(*)",
    "MATCH (a:Item)-[:NEXT]->(:Item)-[:NEXT]->(c:Item) WHERE a.i % 1000 = 0 RETURN a.i, c.i ORDER BY a.i",
    "MATCH (a:Item) WHERE NOT (a)--() RETURN count(*)",
    "MATCH (a:Item) WHERE a.i % 7 = 3 AND (a)-[:NEXT]->() RETURN count(*)",
];

fn run(db: &Database, query: &str) {
    let result = db.execute(query, &Parameters::new()).unwrap();
    println!("{query}\n  {:?}", result.rows());
}

fn exec(db: &Database, query: &str) {
    db.execute(query, &Parameters::new()).unwrap();
}

/// Node IDs are handed out in creation order, so the items fill group 0, group 1, and the
/// start of group 2, and the hub follows them.
fn build(db: &Database) {
    db.create_property_index("item i", IndexTarget::Label("Item"), "i")
        .unwrap();
    db.create_property_index("par k", IndexTarget::RelationshipType("PAR"), "k")
        .unwrap();
    exec(db, "UNWIND range(0, 8999) AS i CREATE (:Item {i: i})");
    exec(db, "CREATE (:Hub {i: -1})");
    exec(
        db,
        "MATCH (a:Item), (b:Item) WHERE a.i % 3 = 0 AND b.i = a.i + 1 CREATE (a)-[:NEXT {w: a.i}]->(b)",
    );
    exec(
        db,
        "MATCH (a:Item), (b:Item) WHERE a.i % 3 = 1 AND b.i = a.i + 1 CREATE (a)-[:NEXT {w: a.i}]->(b)",
    );
    exec(
        db,
        "MATCH (a:Item), (b:Item) WHERE a.i % 3 = 2 AND b.i = a.i + 1 CREATE (a)-[:NEXT {w: a.i}]->(b)",
    );
    exec(
        db,
        "MATCH (h:Hub), (x:Item) WHERE x.i % 9 <> 4 CREATE (h)-[:FAN]->(x)",
    );
    exec(
        db,
        "MATCH (h:Hub), (x:Item) WHERE x.i % 2 = 1 CREATE (x)-[:BACK {w: x.i % 10}]->(h)",
    );
    exec(
        db,
        "MATCH (x:Item) WHERE x.i % 500 = 7 CREATE (x)-[:SELF {w: x.i}]->(x)",
    );
    exec(
        db,
        "MATCH (a:Item {i: 10}), (b:Item {i: 4100}) CREATE (a)-[:PAR {k: 1}]->(b), (a)-[:PAR {k: 2}]->(b), (b)-[:PAR {k: 3}]->(a), (a)-[:PAR {k: 2}]->(a)",
    );
    exec(db, "MATCH ()-[r:NEXT]->() WHERE r.w % 11 = 5 DELETE r");
    exec(
        db,
        "MATCH (:Hub)-[r:FAN]->(x) WHERE x.i >= 4096 AND x.i < 4200 DELETE r",
    );
    exec(db, "MATCH (x:Item) WHERE x.i % 1001 = 6 DETACH DELETE x");
    exec(
        db,
        "MATCH (a:Item {i: 10}), (h:Hub) CREATE (a)-[:FAN]->(h), (h)-[:BACK {w: 99}]->(a)",
    );
    for query in QUERIES {
        run(db, query);
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        panic!("usage: format-9 <path>");
    };
    build(&Database::open(&path).unwrap());
    let mut database = redb::Database::open(&path).unwrap();
    while database.compact(redb::CompactionMode::default()).unwrap() {}
}
