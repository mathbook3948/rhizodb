mod support;

use std::path::Path;
use std::path::PathBuf;

use rhizodb::Database;
use support::*;

const FORMAT_SEVEN: &str = "tests/fixtures/format-7.redb";

const ANSWERS_AT_FORMAT_SEVEN: &[(&str, &str)] = &[
    ("MATCH (n) RETURN count(*)", "[[Integer(52)]]"),
    ("MATCH (n:Person) RETURN count(*)", "[[Integer(6)]]"),
    (
        "MATCH (n:Bulk) RETURN count(*), sum(n.i), min(id(n)), max(id(n))",
        "[[Integer(42), Integer(86394), Integer(16), Integer(4116)]]",
    ),
    (
        "MATCH (n:Admin) RETURN n.name ORDER BY n.name",
        "[[String(\"Edsger\")]]",
    ),
    (
        "MATCH (n) WHERE size(labels(n)) = 0 RETURN id(n), n.note ORDER BY id(n)",
        "[[Integer(4), String(\"bare\")], [Integer(5), Null]]",
    ),
    (
        "MATCH (n:Person) RETURN id(n), labels(n), n.name, n.age ORDER BY id(n)",
        "[[Integer(0), List([String(\"Person\")]), String(\"Ada\"), Integer(36)], \
         [Integer(1), List([String(\"Person\")]), String(\"Charles\"), Integer(41)], \
         [Integer(2), List([String(\"Person\")]), String(\"Grace\"), Integer(29)], \
         [Integer(3), List([String(\"Person\"), String(\"Admin\"), String(\"Owner\")]), String(\"Edsger\"), Null], \
         [Integer(4016), List([String(\"Person\"), String(\"Bulk\")]), String(\"Late\"), Integer(50)], \
         [Integer(4209), List([String(\"Person\")]), String(\"Zed\"), Null]]",
    ),
    (
        "MATCH (a)-[r:KNOWS]->(b) RETURN a.name, r.since, b.name ORDER BY a.name, b.name",
        "[[String(\"Ada\"), Integer(1843), String(\"Charles\")], [String(\"Grace\"), Integer(1952), Null]]",
    ),
    (
        "MATCH (p:Person) WHERE p.age > 30 RETURN p.name ORDER BY p.name",
        "[[String(\"Ada\")], [String(\"Charles\")], [String(\"Late\")]]",
    ),
    (
        "CALL db.index.keyword.queryNodes('doc text', 'graph', 10) YIELD node, score RETURN node.title, score",
        "[[String(\"b\"), Float(0.6462549902128865)], [String(\"a\"), Float(0.47000362924573563)]]",
    ),
    (
        "CALL db.index.vector.queryNodes('doc embedding', [1.0, 0.0, 0.5], 10) YIELD node, score RETURN node.title, score",
        "[[String(\"a\"), Float(0.9999999999999998)], [String(\"b\"), Float(0.5976143046671968)]]",
    ),
    (
        "MATCH (d:Doc) RETURN d.title, d.embedding ORDER BY d.title",
        "[[String(\"a\"), List([Float(1.0), Float(0.0), Float(0.5)])], \
         [String(\"b\"), List([Float(0.1), Float(0.2), Float(0.3)])], [String(\"c\"), Null]]",
    ),
];

fn copy_of_fixture(directory: &Path) -> PathBuf {
    let path = directory.join("graph.redb");
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join(FORMAT_SEVEN),
        &path,
    )
    .unwrap();
    path
}

fn assert_answers(db: &Database) {
    for (query, expected) in ANSWERS_AT_FORMAT_SEVEN {
        assert_eq!(format!("{:?}", run(db, query)), *expected, "{query}");
    }
}

#[test]
fn a_database_of_format_seven_answers_after_the_upgrade_as_it_did_before() {
    let directory = tempfile::tempdir().unwrap();
    let path = copy_of_fixture(directory.path());
    assert_answers(&Database::open(&path).unwrap());
    assert_answers(&Database::open(&path).unwrap());
}

#[test]
fn a_database_of_format_seven_takes_writes_after_the_upgrade() {
    let directory = tempfile::tempdir().unwrap();
    let db = Database::open(copy_of_fixture(directory.path())).unwrap();
    let created = single(&db, "CREATE (n:Person:Admin {name: 'New'}) RETURN id(n)");
    assert_eq!(created, int(4210));
    run(&db, "MATCH (n:Person {name: 'Ada'}) SET n:Admin");
    run(&db, "MATCH (n:Bulk) WHERE n.i < 1000 DETACH DELETE n");
    assert_eq!(
        run(&db, "MATCH (n:Admin) RETURN n.name ORDER BY n.name"),
        [
            vec![string("Ada")],
            vec![string("Edsger")],
            vec![string("New")]
        ]
    );
    assert_eq!(single(&db, "MATCH (n:Bulk) RETURN count(*)"), int(32));
    assert_eq!(single(&db, "MATCH (n:Person) RETURN count(*)"), int(7));
}
