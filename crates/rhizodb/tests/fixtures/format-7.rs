//! Writes the format 7 fixtures through the public API of the last commit that wrote format 7.
//! `generate-format-7.sh` builds it against that commit. Run as
//! `format-7 basic|wide <path>`, it writes the database, prints the answer of each query the
//! upgrade test checks, and compacts the file.

use rhizodb::Database;
use rhizodb::IndexTarget;
use rhizodb::Parameters;
use rhizodb::VectorMetric;

const BASIC_QUERIES: &[&str] = &[
    "MATCH (n) RETURN count(*)",
    "MATCH (n:Person) RETURN count(*)",
    "MATCH (n:Bulk) RETURN count(*), sum(n.i), min(id(n)), max(id(n))",
    "MATCH (n:Admin) RETURN n.name ORDER BY n.name",
    "MATCH (n) WHERE size(labels(n)) = 0 RETURN id(n), n.note ORDER BY id(n)",
    "MATCH (n:Person) RETURN id(n), labels(n), n.name, n.age ORDER BY id(n)",
    "MATCH (a)-[r:KNOWS]->(b) RETURN a.name, r.since, b.name ORDER BY a.name, b.name",
    "MATCH (p:Person) WHERE p.age > 30 RETURN p.name ORDER BY p.name",
    "CALL db.index.keyword.queryNodes('doc text', 'graph', 10) YIELD node, score RETURN node.title, score",
    "CALL db.index.vector.queryNodes('doc embedding', [1.0, 0.0, 0.5], 10) YIELD node, score RETURN node.title, score",
    "MATCH (d:Doc) RETURN d.title, d.embedding ORDER BY d.title",
];

const WIDE_QUERIES: &[&str] = &[
    "MATCH (n) RETURN count(*), sum(n.i), sum(n.i * n.i), min(id(n)), max(id(n))",
    "MATCH (n) WITH labels(n) AS l RETURN count(DISTINCT l)",
    "MATCH (n) RETURN size(labels(n)) AS s, count(*) ORDER BY s",
    "MATCH (n) WHERE size(labels(n)) = 0 RETURN id(n), n.i ORDER BY id(n)",
    "MATCH (n:Wide) RETURN count(*), sum(n.i), sum(n.i * n.i)",
    "MATCH (n:L0) RETURN count(*), sum(n.i), sum(n.i * n.i)",
    "MATCH (n:L1) RETURN count(*), sum(n.i), sum(n.i * n.i)",
    "MATCH (n:L2) RETURN count(*), sum(n.i), sum(n.i * n.i)",
    "MATCH (n:L3) RETURN count(*), sum(n.i), sum(n.i * n.i)",
    "MATCH (n:L4) RETURN count(*), sum(n.i), sum(n.i * n.i)",
    "MATCH (n:L5) RETURN count(*), sum(n.i), sum(n.i * n.i)",
    "MATCH (n:L6) RETURN count(*), sum(n.i), sum(n.i * n.i)",
    "MATCH (n:L7) RETURN count(*), sum(n.i), sum(n.i * n.i)",
    "MATCH (n:L8) RETURN count(*), sum(n.i), sum(n.i * n.i)",
    "MATCH (n:Sparse) RETURN id(n), labels(n) ORDER BY id(n)",
    "MATCH (n:Gone) RETURN count(*)",
    "MATCH (n) WHERE id(n) IN [4094, 4095, 4096, 8191, 8192, 8193, 12287, 12288, 12289] RETURN id(n), n.i, labels(n) ORDER BY id(n)",
    "MATCH (n:Wide {i: 777}) RETURN id(n), labels(n)",
    "MATCH (a)-[r:LINK]->(b) RETURN id(a), r.w, id(b) ORDER BY r.w",
];

fn run(db: &Database, query: &str) {
    let result = db.execute(query, &Parameters::new()).unwrap();
    println!("{query}\n  {:?}", result.rows());
}

fn exec(db: &Database, query: &str) {
    db.execute(query, &Parameters::new()).unwrap();
}

fn basic(db: &Database) {
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
    exec(
        db,
        "CREATE (:Person {name: 'Ada', age: 36})-[:KNOWS {since: 1843}]->(:Person:Admin {name: 'Charles', age: 41}),
                (:Person {name: 'Grace', age: 29}), (:Person:Admin:Owner {name: 'Edsger'}), ({note: 'bare'}), ()",
    );
    exec(
        db,
        "CREATE (:Doc {title: 'a', text: 'a graph store', embedding: [1.0, 0.0, 0.5]}),
                (:Doc {title: 'b', text: 'graph graph words', embedding: [0.1, 0.2, 0.3]}),
                (:Doc:Draft {title: 'c', text: 'no match here'})",
    );
    exec(db, "UNWIND range(0, 4199) AS i CREATE (:Bulk {i: i})");
    exec(db, "MATCH (b:Bulk) WHERE b.i % 100 <> 7 DELETE b");
    exec(
        db,
        "MATCH (a:Person {name: 'Grace'}), (b:Bulk {i: 4107}) CREATE (a)-[:KNOWS {since: 1952}]->(b)",
    );
    exec(
        db,
        "MATCH (b:Bulk {i: 4007}) SET b:Person, b.name = 'Late', b.age = 50",
    );
    exec(db, "MATCH (p:Person {name: 'Charles'}) REMOVE p:Admin");
    exec(db, "MATCH (n {note: 'bare'}) CREATE (:Person {name: 'Zed'})");
    for query in BASIC_QUERIES {
        run(db, query);
    }
}

/// Node IDs are handed out in creation order, so each statement below fills the IDs after the
/// last one. Group 0 holds more than 256 label sets, group 1 loses every node, group 2 keeps a
/// few nodes at its edges, and group 3 starts with the last two nodes.
fn wide(db: &Database) {
    db.create_property_index("wide i", IndexTarget::Label("Wide"), "i")
        .unwrap();
    exec(db, "UNWIND range(0, 1199) AS i CREATE (:Wide {i: i})");
    for bit in 0..9 {
        let step = 1 << bit;
        exec(
            db,
            &format!(
                "MATCH (n:Wide) WHERE n.i % {} >= {step} SET n:L{bit}",
                2 * step
            ),
        );
    }
    exec(
        db,
        "MATCH (n:Wide) WHERE n.i % 11 = 5 OR n.i = 1024 REMOVE n:Wide",
    );
    exec(db, "MATCH (n) WHERE n.i % 7 = 3 DELETE n");
    exec(db, "UNWIND range(1200, 4094) AS i CREATE (:Gone {i: i})");
    exec(db, "CREATE (:Wide:L8 {i: 4095})");
    exec(db, "UNWIND range(4096, 8191) AS i CREATE (:Gone {i: i})");
    exec(db, "MATCH (n:Gone) DELETE n");
    exec(
        db,
        "UNWIND range(8192, 12287) AS i CREATE (:Sparse:Wide {i: i})",
    );
    exec(
        db,
        "MATCH (n:Sparse) WHERE NOT (n.i IN [8192, 8193, 12286, 12287] OR n.i % 1000 = 0) DELETE n",
    );
    exec(db, "MATCH (n:Sparse {i: 10000}) REMOVE n:Sparse, n:Wide");
    exec(db, "CREATE (:Tail {i: 12288}), (:Tail:L0 {i: 12289})");
    exec(
        db,
        "MATCH (a:Wide {i: 1}), (b:Tail {i: 12288}) CREATE (a)-[:LINK {w: 1}]->(b)",
    );
    exec(
        db,
        "MATCH (a:Sparse {i: 8192}), (b:Wide {i: 4095}) CREATE (a)-[:LINK {w: 2}]->(b)",
    );
    for query in WIDE_QUERIES {
        run(db, query);
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(kind), Some(path)) = (args.next(), args.next()) else {
        panic!("usage: format-7 basic|wide <path>");
    };
    {
        let db = Database::open(&path).unwrap();
        match kind.as_str() {
            "basic" => basic(&db),
            "wide" => wide(&db),
            other => panic!("unknown fixture {other}"),
        }
    }
    let mut database = redb::Database::open(&path).unwrap();
    while database.compact(redb::CompactionMode::default()).unwrap() {}
}
