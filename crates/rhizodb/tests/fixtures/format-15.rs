//! Writes the format 15 fixture through the public API of the last commit that wrote format 15.
//! `generate-format-15.sh` builds it against that commit. Run as `format-15 <path>`, it writes
//! the database, compacts it, closes it, and prints each query the upgrade test checks, each
//! followed by its answer on an indented line, which the script keeps in `format-15.answers`.
//! It holds the relationships of the format 11 fixture, each record stored whole in its chunk,
//! over hundreds of chunks with gaps from deleted relationships, records updated after they were
//! written, records without properties and with hundreds of them, every kind of stored value as
//! a relationship property, and a property index on a relationship type.

use rhizodb::Database;
use rhizodb::IndexTarget;
use rhizodb::Parameters;

const QUERIES: &[&str] = &[
    "MATCH (n) RETURN count(*)",
    "MATCH ()-[r]->() RETURN count(*), min(id(r)), max(id(r)), sum(id(r))",
    "MATCH (a)-[r]->(b) RETURN sum(id(a)), sum(id(b)), count(DISTINCT type(r))",
    "MATCH ()-[r:NEXT]->() RETURN count(*), sum(r.w), min(r.w), max(r.w)",
    "MATCH (a:Item)-[r:NEXT]->(b:Item) WHERE a.i >= 16380 AND a.i < 16390 RETURN a.i, b.i, r.w ORDER BY a.i",
    "MATCH (a:Item)-[r:NEXT]->(b:Item) WHERE a.i < 3 OR a.i > 16996 RETURN a.i, b.i, r.w ORDER BY a.i",
    "MATCH ()-[r:EVERY]->() RETURN r.b, r.i, r.f, r.s, r.l, r.d, r.t, r.neg, r.big, r.e ORDER BY r.i",
    "MATCH ()-[r:EVERY]->() RETURN keys(r) ORDER BY r.i",
    "MATCH ()-[r]->() WHERE type(r) STARTS WITH 'T' RETURN type(r) AS t, count(*), sum(r.k) ORDER BY t",
    "MATCH ()-[r:MANY]->() RETURN size(keys(r)), r.p0, r.p127, r.p128, r.p200",
    "MATCH (a)-[r:BARE]->(b) RETURN a.i, b.i, keys(r) ORDER BY a.i",
    "MATCH ()-[r:PAR {k: 2}]->() RETURN count(*)",
    "MATCH (a:Item {i: 10})-[r]-(b) RETURN id(r), type(r), r.k, b.i ORDER BY id(r)",
    "MATCH (a)-[r:SELF]->(a) RETURN a.i, r.w ORDER BY a.i",
    "MATCH (a:Item)-[:NEXT*1..3]->(b:Item {i: 128}) RETURN a.i ORDER BY a.i",
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
    db.create_property_index("par k", IndexTarget::RelationshipType("PAR"), "k")
        .unwrap();
    exec(db, "UNWIND range(0, 16999) AS i CREATE (:Item {i: i})");
    exec(
        db,
        "MATCH (a:Item), (b:Item) WHERE b.i = a.i + 1 AND a.i % 2 = 0 CREATE (a)-[:NEXT {w: a.i - 8000}]->(b)",
    );
    exec(
        db,
        "MATCH (a:Item), (b:Item) WHERE b.i = a.i + 1 AND a.i % 2 = 1 CREATE (a)-[:NEXT {w: a.i * 1000000}]->(b)",
    );
    exec(
        db,
        "MATCH (a:Item {i: 1}), (b:Item {i: 16999}) CREATE (a)-[:EVERY {b: true, i: 1, f: -0.0, s: 'text', l: [1, 2, 3], d: date('2024-02-29'), t: localdatetime('2020-01-01T10:00:00'), neg: -9223372036854775808, big: 9223372036854775807, e: ''}]->(b)",
    );
    exec(
        db,
        "MATCH (a:Item {i: 16999}), (b:Item {i: 1}) CREATE (a)-[:EVERY {b: false, i: 2, f: 1.5e300, s: '한국어', l: ['a', 'b'], d: date('-0001-01-01'), t: duration('P1Y2M3DT4H'), neg: -1, big: 128, e: [1.5, 2.5]}]->(b)",
    );
    for index in 0..140 {
        exec(
            db,
            &format!(
                "MATCH (a:Item {{i: {}}}), (b:Item {{i: {}}}) CREATE (a)-[:T{index} {{k: {index}}}]->(b)",
                index * 100,
                16999 - index * 100
            ),
        );
    }
    let many: Vec<String> = (0..210).map(|key| format!("p{key}: {key}")).collect();
    exec(
        db,
        &format!(
            "MATCH (a:Item {{i: 5}}), (b:Item {{i: 6}}) CREATE (a)-[:MANY {{{}}}]->(b)",
            many.join(", ")
        ),
    );
    exec(
        db,
        "MATCH (a:Item), (b:Item) WHERE a.i % 4000 = 3 AND b.i = a.i + 2 CREATE (a)-[:BARE]->(b)",
    );
    exec(
        db,
        "MATCH (a:Item {i: 10}), (b:Item {i: 16383}) CREATE (a)-[:PAR {k: 1}]->(b), (a)-[:PAR {k: 2}]->(b), (b)-[:PAR {k: 3}]->(a), (a)-[:PAR {k: 2}]->(a)",
    );
    exec(
        db,
        "MATCH (x:Item) WHERE x.i % 3000 = 7 CREATE (x)-[:SELF {w: x.i}]->(x)",
    );
    exec(db, "MATCH ()-[r:NEXT]->() WHERE r.w % 11 = 5 DELETE r");
    exec(
        db,
        "MATCH ()-[r:NEXT]->() WHERE r.w % 13 = 4 SET r.w = r.w + 1, r.extra = 'x'",
    );
    exec(db, "MATCH (x:Item) WHERE x.i % 5001 = 9 DETACH DELETE x");
    for query in QUERIES {
        run(db, query);
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        panic!("usage: format-15 <path>");
    };
    let db = Database::open(&path).unwrap();
    build(&db);
    db.compact().unwrap();
}
