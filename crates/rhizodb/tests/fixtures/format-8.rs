//! Writes the format 8 fixtures through the public API of the last commit that wrote format 8.
//! `generate-format-8.sh` builds it against that commit. Run as
//! `format-8 typed|db-file <path>`, it writes the database and prints each query the upgrade
//! test checks, each followed by its answer on an indented line, which the script keeps in
//! `format-8.answers`. `typed` holds property values of every type across several row
//! groups and is compacted. `db-file` is the undamaged file of the `db_file` fuzz target at that
//! commit, byte for byte, so the damages the fuzzer found on it replay on the same bytes.

use rhizodb::Database;
use rhizodb::IndexTarget;
use rhizodb::Parameters;
use rhizodb::VectorMetric;

const TYPED_QUERIES: &[&str] = &[
    "MATCH (n) RETURN count(*)",
    "MATCH (n:Bulk) RETURN count(*), sum(n.i), min(n.i), max(n.i), min(id(n)), max(id(n))",
    "MATCH (n:Bulk) RETURN count(n.s), count(n.f), count(n.b), count(n.m)",
    "MATCH (n:Bulk) WHERE n.b RETURN count(*), sum(n.f)",
    "MATCH (n:Bulk) WHERE n.i < 60 OR n.i >= 8190 RETURN n.m AS m, count(*) ORDER BY m",
    "MATCH (n:Bulk) WHERE n.i >= 4090 AND n.i < 4100 RETURN id(n), n.i, n.f, n.s, n.b, n.m ORDER BY id(n)",
    "MATCH (n:Bulk) WHERE n.s IS NOT NULL RETURN n.s ORDER BY n.s DESC LIMIT 5",
    "MATCH (n:Odd) RETURN id(n), n ORDER BY id(n)",
    "MATCH (n:Person) RETURN n.name, n.age ORDER BY n.name",
    "MATCH (n:Person) WHERE n.age > 30 RETURN n.name ORDER BY n.name",
    "MATCH (a)-[r:KNOWS]->(b) RETURN a.name, r.since, b.name ORDER BY a.name",
    "CALL db.index.keyword.queryNodes('doc text', 'graph', 10) YIELD node, score RETURN node.title, score",
    "CALL db.index.vector.queryNodes('doc embedding', [1.0, 0.0, 0.5], 10) YIELD node, score RETURN node.title, score",
];

const DB_FILE_SETUP: &[&str] = &[
    "INSERT (:Person {name: 'Ada', age: 36, tags: ['a', 'b'], score: 1.5, \
     born: date('1815-12-10')})-[:KNOWS {since: 1833}]->(:Person {name: 'Charles', age: 79})",
    "UNWIND range(1, 300) AS i INSERT (:Item {i: i, text: 'word ' + toString(i), \
     vec: [toFloat(i), 1.0], blob: 'x' + toString(i * 7919)})",
    "MATCH (a:Item), (b:Item) WHERE b.i = a.i + 1 INSERT (a)-[:NEXT {w: a.i}]->(b)",
    "MATCH (n:Item) WHERE n.i % 7 = 0 DETACH DELETE n",
    "MATCH (n:Item) WHERE n.i % 5 = 0 SET n:Five REMOVE n.blob",
];

fn run(db: &Database, query: &str) {
    let result = db.execute(query, &Parameters::new()).unwrap();
    println!("{query}\n  {:?}", result.rows());
}

fn exec(db: &Database, query: &str) {
    db.execute(query, &Parameters::new()).unwrap();
}

/// Node IDs are handed out in creation order. The bulk nodes fill group 0, group 1, and the
/// start of group 2; group 1 then loses most of its nodes and every value of `s`, and `m` holds
/// a value of another type in every few nodes.
fn typed(db: &Database) {
    db.create_keyword_index("doc text", IndexTarget::Label("Doc"), &["text"])
        .unwrap();
    db.create_vector_index(
        "doc embedding",
        IndexTarget::Label("Doc"),
        "embedding",
        3,
        VectorMetric::Cosine,
    )
    .unwrap();
    db.create_property_index("person age", IndexTarget::Label("Person"), "age")
        .unwrap();
    db.create_property_index("bulk m", IndexTarget::Label("Bulk"), "m")
        .unwrap();
    exec(
        db,
        "UNWIND range(0, 8299) AS i CREATE (:Bulk {i: i, f: toFloat(i) / 4.0, s: 'v' + toString(i), b: i % 3 = 0})",
    );
    exec(db, "MATCH (n:Bulk) WHERE n.i % 5 = 1 SET n.m = n.i");
    exec(db, "MATCH (n:Bulk) WHERE n.i % 5 = 2 SET n.m = toString(n.i)");
    exec(db, "MATCH (n:Bulk) WHERE n.i % 5 = 3 SET n.m = toFloat(n.i) + 0.5");
    exec(db, "MATCH (n:Bulk) WHERE n.i % 50 = 4 SET n.m = [n.i, 'x']");
    exec(db, "MATCH (n:Bulk) WHERE n.i % 50 = 9 SET n.m = n.i % 2 = 0");
    exec(
        db,
        "MATCH (n:Bulk) WHERE n.i >= 4096 AND n.i < 8192 AND n.i % 97 <> 0 DELETE n",
    );
    exec(db, "MATCH (n:Bulk) WHERE n.i >= 4096 AND n.i < 8192 REMOVE n.s");
    exec(db, "MATCH (n:Bulk) WHERE n.i % 1000 = 17 REMOVE n.f, n.b");
    exec(db, "MATCH (n:Bulk {i: 4095}) SET n.s = ''");
    exec(db, "MATCH (n:Bulk {i: 8192}) SET n.s = 'ünïcödé ✓'");
    exec(
        db,
        "CREATE (:Odd {zero: -0.0, big: 9223372036854775807, small: -9223372036854775807 - 1, \
         tiny: 0.000001, no: false, empty: '', list: [], nested: [[1, 2], ['a']], day: date('2024-02-29')}), \
         (:Odd {zero: 0.0, big: 1, no: true})",
    );
    exec(
        db,
        "CREATE (:Person {name: 'Ada', age: 36})-[:KNOWS {since: 1843}]->(:Person {name: 'Charles', age: 41}), \
         (:Person {name: 'Grace', age: 29}), (:Person {name: 'Edsger'})",
    );
    exec(
        db,
        "CREATE (:Doc {title: 'a', text: 'a graph store', embedding: [1.0, 0.0, 0.5]}), \
         (:Doc {title: 'b', text: 'graph graph words', embedding: [0.1, 0.2, 0.3]}), \
         (:Doc {title: 'c', text: 'no match here'})",
    );
    for query in TYPED_QUERIES {
        run(db, query);
    }
}

/// The setup of `tests/support/db_file.rs` at the pinned commit, which writes the file the
/// `db_file` fuzz target damages.
fn db_file(db: &Database) {
    db.create_property_index("item_i", IndexTarget::Label("Item"), "i")
        .unwrap();
    db.create_keyword_index("text", IndexTarget::Label("Item"), &["text"])
        .unwrap();
    db.create_vector_index(
        "vec",
        IndexTarget::Label("Item"),
        "vec",
        2,
        VectorMetric::Cosine,
    )
    .unwrap();
    db.create_property_index(
        "knows_since",
        IndexTarget::RelationshipType("KNOWS"),
        "since",
    )
    .unwrap();
    for query in DB_FILE_SETUP {
        exec(db, query);
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(kind), Some(path)) = (args.next(), args.next()) else {
        panic!("usage: format-8 typed|db-file <path>");
    };
    {
        let db = Database::open(&path).unwrap();
        match kind.as_str() {
            "typed" => typed(&db),
            "db-file" => db_file(&db),
            other => panic!("unknown fixture {other}"),
        }
    }
    if kind == "typed" {
        let mut database = redb::Database::open(&path).unwrap();
        while database.compact(redb::CompactionMode::default()).unwrap() {}
    }
}
