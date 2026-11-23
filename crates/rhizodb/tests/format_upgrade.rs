mod support;

use std::path::Path;
use std::path::PathBuf;

use rhizodb::Database;
use support::*;

const FORMAT_SEVEN: &str = "tests/fixtures/format-7.redb";
const FORMAT_SEVEN_WIDE: &str = "tests/fixtures/format-7-wide.redb";

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

const WIDE_ANSWERS_AT_FORMAT_SEVEN: &[(&str, &str)] = &[
    (
        "MATCH (n) RETURN count(*), sum(n.i), sum(n.i * n.i), min(id(n)), max(id(n))",
        "[[Integer(1040), Integer(728772), Integer(1694646854), Integer(0), Integer(12289)]]",
    ),
    (
        "MATCH (n) WITH labels(n) AS l RETURN count(DISTINCT l)",
        "[[Integer(601)]]",
    ),
    (
        "MATCH (n) RETURN size(labels(n)) AS s, count(*) ORDER BY s",
        "[[Integer(0), Integer(2)], [Integer(1), Integer(6)], [Integer(2), Integer(32)], [Integer(3), Integer(98)], [Integer(4), Integer(196)], [Integer(5), Integer(254)], [Integer(6), Integer(228)], [Integer(7), Integer(155)], [Integer(8), Integer(52)], [Integer(9), Integer(16)], [Integer(10), Integer(1)]]",
    ),
    (
        "MATCH (n) WHERE size(labels(n)) = 0 RETURN id(n), n.i ORDER BY id(n)",
        "[[Integer(1024), Integer(1024)], [Integer(10000), Integer(10000)]]",
    ),
    (
        "MATCH (n:Wide) RETURN count(*), sum(n.i), sum(n.i * n.i)",
        "[[Integer(943), Integer(637728), Integer(1247494698)]]",
    ),
    (
        "MATCH (n:L0) RETURN count(*), sum(n.i), sum(n.i * n.i)",
        "[[Integer(515), Integer(320861), Integer(397877867)]]",
    ),
    (
        "MATCH (n:L1) RETURN count(*), sum(n.i), sum(n.i * n.i)",
        "[[Integer(514), Integer(309173), Integer(247580149)]]",
    ),
    (
        "MATCH (n:L2) RETURN count(*), sum(n.i), sum(n.i * n.i)",
        "[[Integer(516), Integer(310374), Integer(249023486)]]",
    ),
    (
        "MATCH (n:L3) RETURN count(*), sum(n.i), sum(n.i * n.i)",
        "[[Integer(514), Integer(310378), Integer(249031188)]]",
    ),
    (
        "MATCH (n:L4) RETURN count(*), sum(n.i), sum(n.i * n.i)",
        "[[Integer(507), Integer(304361), Integer(241789749)]]",
    ),
    (
        "MATCH (n:L5) RETURN count(*), sum(n.i), sum(n.i * n.i)",
        "[[Integer(509), Integer(309182), Integer(247585994)]]",
    ),
    (
        "MATCH (n:L6) RETURN count(*), sum(n.i), sum(n.i * n.i)",
        "[[Integer(493), Integer(299383), Integer(235625171)]]",
    ),
    (
        "MATCH (n:L7) RETURN count(*), sum(n.i), sum(n.i * n.i)",
        "[[Integer(480), Integer(301714), Integer(239925200)]]",
    ),
    (
        "MATCH (n:L8) RETURN count(*), sum(n.i), sum(n.i * n.i)",
        "[[Integer(441), Integer(285439), Integer(227935157)]]",
    ),
    (
        "MATCH (n:Sparse) RETURN id(n), labels(n) ORDER BY id(n)",
        "[[Integer(8192), List([String(\"Wide\"), String(\"Sparse\")])], [Integer(8193), List([String(\"Wide\"), String(\"Sparse\")])], [Integer(9000), List([String(\"Wide\"), String(\"Sparse\")])], [Integer(11000), List([String(\"Wide\"), String(\"Sparse\")])], [Integer(12000), List([String(\"Wide\"), String(\"Sparse\")])], [Integer(12286), List([String(\"Wide\"), String(\"Sparse\")])], [Integer(12287), List([String(\"Wide\"), String(\"Sparse\")])]]",
    ),
    ("MATCH (n:Gone) RETURN count(*)", "[[Integer(0)]]"),
    (
        "MATCH (n) WHERE id(n) IN [4094, 4095, 4096, 8191, 8192, 8193, 12287, 12288, 12289] RETURN id(n), n.i, labels(n) ORDER BY id(n)",
        "[[Integer(4095), Integer(4095), List([String(\"Wide\"), String(\"L8\")])], [Integer(8192), Integer(8192), List([String(\"Wide\"), String(\"Sparse\")])], [Integer(8193), Integer(8193), List([String(\"Wide\"), String(\"Sparse\")])], [Integer(12287), Integer(12287), List([String(\"Wide\"), String(\"Sparse\")])], [Integer(12288), Integer(12288), List([String(\"Tail\")])], [Integer(12289), Integer(12289), List([String(\"L0\"), String(\"Tail\")])]]",
    ),
    (
        "MATCH (n:Wide {i: 777}) RETURN id(n), labels(n)",
        "[[Integer(777), List([String(\"Wide\"), String(\"L0\"), String(\"L3\"), String(\"L8\")])]]",
    ),
    (
        "MATCH (a)-[r:LINK]->(b) RETURN id(a), r.w, id(b) ORDER BY r.w",
        "[[Integer(1), Integer(1), Integer(12288)], [Integer(8192), Integer(2), Integer(4095)]]",
    ),
];

fn copy_of_fixture(directory: &Path) -> PathBuf {
    copy_of(FORMAT_SEVEN, directory)
}

fn copy_of(fixture: &str, directory: &Path) -> PathBuf {
    let path = directory.join("graph.redb");
    std::fs::copy(Path::new(env!("CARGO_MANIFEST_DIR")).join(fixture), &path).unwrap();
    path
}

fn assert_answers(db: &Database) {
    assert_answers_of(db, ANSWERS_AT_FORMAT_SEVEN);
}

fn assert_answers_of(db: &Database, answers: &[(&str, &str)]) {
    for (query, expected) in answers {
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

#[test]
fn a_database_of_format_seven_with_many_label_sets_and_groups_answers_after_the_upgrade_as_it_did_before()
 {
    let directory = tempfile::tempdir().unwrap();
    let path = copy_of(FORMAT_SEVEN_WIDE, directory.path());
    assert_answers_of(
        &Database::open(&path).unwrap(),
        WIDE_ANSWERS_AT_FORMAT_SEVEN,
    );
    assert_answers_of(
        &Database::open(&path).unwrap(),
        WIDE_ANSWERS_AT_FORMAT_SEVEN,
    );
}

const FORMAT_EIGHT: &str = "tests/fixtures/format-8.redb";

/// Each query the format 8 generator ran, with the answer it printed at format 8.
fn answers_at_format_eight() -> Vec<(String, String)> {
    let text = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/format-8.answers"),
    )
    .unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len() % 2, 0, "each query has one answer line");
    lines
        .chunks(2)
        .map(|pair| {
            let answer = pair[1]
                .strip_prefix("  ")
                .expect("an answer line is indented");
            (pair[0].to_owned(), answer.to_owned())
        })
        .collect()
}

fn assert_format_eight_answers(db: &Database) {
    let answers = answers_at_format_eight();
    assert_eq!(answers.len(), 13);
    for (query, expected) in &answers {
        assert_eq!(format!("{:?}", run(db, query)), *expected, "{query}");
    }
}

#[test]
fn a_database_of_format_eight_answers_after_the_upgrade_as_it_did_before() {
    let directory = tempfile::tempdir().unwrap();
    let path = copy_of(FORMAT_EIGHT, directory.path());
    assert_format_eight_answers(&Database::open(&path).unwrap());
    assert_format_eight_answers(&Database::open(&path).unwrap());
}

#[test]
fn a_database_of_format_eight_takes_writes_of_every_type_after_the_upgrade() {
    let directory = tempfile::tempdir().unwrap();
    let path = copy_of(FORMAT_EIGHT, directory.path());
    {
        let db = Database::open(&path).unwrap();
        run(
            &db,
            "MATCH (n:Bulk) WHERE n.i % 10 = 0 SET n.i = toString(n.i)",
        );
        // A string `i` compares with no integer, so the nodes made strings above stay.
        run(&db, "MATCH (n:Bulk) WHERE n.i >= 8000 DETACH DELETE n");
        run(&db, "MATCH (n:Odd) SET n.zero = 0.0, n.new = [1.5]");
        run(&db, "MATCH (n:Person {name: 'Edsger'}) SET n.age = 31");
        run(&db, "UNWIND range(1, 5000) AS i CREATE (:Late {i: i})");
    }
    let db = Database::open(&path).unwrap();
    assert_eq!(
        run(
            &db,
            "MATCH (n:Bulk) WHERE n.i STARTS WITH '' RETURN count(*)"
        ),
        [vec![int(424)]]
    );
    assert_eq!(
        run(&db, "MATCH (n:Bulk) RETURN count(*), max(id(n))"),
        [vec![int(4146), int(8290)]]
    );
    assert_eq!(
        run(&db, "MATCH (n:Odd) RETURN n.zero, n.new ORDER BY id(n)"),
        [
            vec![float(0.0), list(vec![float(1.5)])],
            vec![float(0.0), list(vec![float(1.5)])]
        ]
    );
    assert_eq!(
        run(
            &db,
            "MATCH (n:Person) WHERE n.age > 30 RETURN n.name ORDER BY n.name"
        ),
        [
            vec![string("Ada")],
            vec![string("Charles")],
            vec![string("Edsger")]
        ]
    );
    assert_eq!(
        run(&db, "MATCH (n:Late) RETURN count(*), sum(n.i), min(id(n))"),
        [vec![int(5000), int(12_502_500), int(8309)]]
    );
}
