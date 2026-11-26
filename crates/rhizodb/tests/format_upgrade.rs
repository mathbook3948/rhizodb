mod support;

use std::path::Path;
use std::path::PathBuf;

use rhizodb::Database;
use rhizodb::Value;
use support::*;

/// Each query of an answer file in `tests/fixtures`, with the answer the fixture's own commit
/// printed.
fn answers_in(file: &str) -> Vec<(String, String)> {
    let text = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(file),
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

const ENGINE_ONE_ONE: &str = "tests/fixtures/engine-1-1.rhizodb";

fn assert_engine_fixture_answers(db: &Database) {
    let answers = answers_in("engine-1-1.answers");
    assert_eq!(answers.len(), 6);
    for (query, expected) in &answers {
        assert_eq!(format!("{:?}", run(db, query)), *expected, "{query}");
    }
}

#[test]
fn an_engine_file_with_an_open_extent_per_kind_of_block_answers_takes_writes_and_compacts() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph.rhizodb");
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join(ENGINE_ONE_ONE),
        &path,
    )
    .unwrap();
    let before = std::fs::metadata(&path).unwrap().len();
    {
        let db = Database::open(&path).unwrap();
        assert_engine_fixture_answers(&db);
        db.check().unwrap();
        db.execute(
            "UNWIND range(3000, 3999) AS i CREATE (:Item {i: i, text: 'new ' + reduce(s = '', k IN range(1, 300) | s + toString(k))})",
            &rhizodb::Parameters::new(),
        )
        .unwrap();
    }
    let db = Database::open(&path).unwrap();
    db.check().unwrap();
    assert_eq!(
        single(&db, "MATCH (n:Item) WHERE n.i >= 3000 RETURN count(*)"),
        int(1000)
    );
    db.execute(
        "MATCH (n:Item) WHERE n.i >= 3000 DETACH DELETE n",
        &rhizodb::Parameters::new(),
    )
    .unwrap();
    db.compact().unwrap();
    db.check().unwrap();
    assert_engine_fixture_answers(&db);
    drop(db);
    assert!(std::fs::metadata(&path).unwrap().len() < before);
    assert_engine_fixture_answers(&Database::open(&path).unwrap());
}

const ENGINE_ONE_ONE_HOLES: &str = "tests/fixtures/engine-1-1-holes.rhizodb";

fn assert_engine_holes_fixture_answers(db: &Database) {
    let answers = answers_in("engine-1-1-holes.answers");
    assert_eq!(answers.len(), 5);
    for (query, expected) in &answers {
        assert_eq!(format!("{:?}", run(db, query)), *expected, "{query}");
    }
}

#[test]
fn an_engine_file_that_only_counts_live_blocks_finds_them_and_fills_its_dead_ones() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph.rhizodb");
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join(ENGINE_ONE_ONE_HOLES),
        &path,
    )
    .unwrap();
    let before = std::fs::metadata(&path).unwrap().len();
    {
        let db = Database::open(&path).unwrap();
        db.check().unwrap();
        assert_engine_holes_fixture_answers(&db);
        let stats = db.stats().unwrap();
        assert!(
            stats.live_bytes + (64 << 10) < stats.file_bytes,
            "the fixture holds no dead blocks: {stats:?}"
        );
        db.execute(
            "UNWIND range(6000, 6299) AS i CREATE (:Item {i: i, age: 1})",
            &rhizodb::Parameters::new(),
        )
        .unwrap();
    }
    let db = Database::open(&path).unwrap();
    db.check().unwrap();
    let filled = db.stats().unwrap();
    assert!(
        filled.file_bytes <= before,
        "writes that fit in dead blocks grew the file from {before}: {filled:?}"
    );
    db.execute(
        "MATCH (n:Item) WHERE n.i >= 6000 DETACH DELETE n",
        &rhizodb::Parameters::new(),
    )
    .unwrap();
    assert_engine_holes_fixture_answers(&db);
    drop(db);
    let reopened = Database::open(&path).unwrap();
    reopened.check().unwrap();
    assert_engine_holes_fixture_answers(&reopened);
}

const FORMAT_THIRTEEN: &str = "tests/fixtures/format-13.db";

fn copy_of_engine_file(fixture: &str, directory: &Path) -> PathBuf {
    let path = directory.join("graph.db");
    std::fs::copy(Path::new(env!("CARGO_MANIFEST_DIR")).join(fixture), &path).unwrap();
    path
}

#[test]
fn a_database_of_format_thirteen_answers_after_the_upgrade_as_it_did_before() {
    let directory = tempfile::tempdir().unwrap();
    let path = copy_of_engine_file(FORMAT_THIRTEEN, directory.path());
    let answers = answers_in("format-13.answers");
    assert_eq!(answers.len(), 15);
    for _ in 0..2 {
        let db = Database::open(&path).unwrap();
        for (query, expected) in &answers {
            assert_eq!(format!("{:?}", run(&db, query)), *expected, "{query}");
        }
    }
}

#[test]
fn a_database_of_format_thirteen_keeps_its_keyword_indexes_exact_through_writes_after_the_upgrade()
{
    let directory = tempfile::tempdir().unwrap();
    let path = copy_of_engine_file(FORMAT_THIRTEEN, directory.path());
    {
        let db = Database::open(&path).unwrap();
        run(
            &db,
            "MATCH (d:Doc) WHERE d.i % 31 = 3 SET d.text = 'graph again and again graph'",
        );
        run(&db, "MATCH (d:Doc) WHERE d.i % 37 = 6 DETACH DELETE d");
        run(
            &db,
            "UNWIND range(2000, 2300) AS i CREATE (:Doc {i: i, title: 'new ' + toString(i), text: 'graph store ocean quokka'})",
        );
        run(
            &db,
            "MATCH (a:Doc {i: 2000}), (b:Doc {i: 2001}) CREATE (a)-[:NOTE {k: 9999, note: 'friend of the bridge'}]->(b)",
        );
    }
    let db = Database::open(&path).unwrap();
    db.create_keyword_index(
        "doc text again",
        rhizodb::IndexTarget::Label("Doc"),
        &["title", "text"],
    )
    .unwrap();
    db.create_keyword_index(
        "note text again",
        rhizodb::IndexTarget::RelationshipType("NOTE"),
        &["note"],
    )
    .unwrap();
    for (procedure, element, index, text) in [
        ("queryNodes", "node", "doc text", "graph"),
        ("queryNodes", "node", "doc text", "quokka"),
        ("queryNodes", "node", "doc text", "stor*"),
        ("queryNodes", "node", "doc text", "\"graph store\" ocean"),
        ("queryNodes", "node", "doc text", "그래프"),
        ("queryNodes", "node", "doc text", "again"),
        ("queryRelationships", "relationship", "note text", "bridge"),
        (
            "queryRelationships",
            "relationship",
            "note text",
            "\"friend of\"",
        ),
    ] {
        let search = |name: &str| {
            run(
                &db,
                &format!(
                    "CALL db.index.keyword.{procedure}('{name}', '{text}', 5000) YIELD {element}, score RETURN id({element}), score"
                ),
            )
        };
        let upgraded = search(index);
        assert!(!upgraded.is_empty(), "{index} {text}");
        assert_eq!(
            upgraded,
            search(&format!("{index} again")),
            "{index} {text}"
        );
    }
    assert_eq!(
        single(
            &db,
            "CALL db.index.keyword.queryNodes('doc text', 'gone', 10) YIELD node RETURN count(*)"
        ),
        int(0)
    );
}

const FORMAT_ELEVEN: &str = "tests/fixtures/format-11.db";

fn assert_relationship_fixture_answers(db: &Database, answers: &str) {
    let answers = answers_in(answers);
    assert_eq!(answers.len(), 15);
    for (query, expected) in &answers {
        assert_eq!(format!("{:?}", run(db, query)), *expected, "{query}");
    }
}

#[test]
fn a_database_of_format_eleven_answers_after_the_upgrade_as_it_did_before() {
    let directory = tempfile::tempdir().unwrap();
    let path = copy_of_engine_file(FORMAT_ELEVEN, directory.path());
    assert_relationship_fixture_answers(&Database::open(&path).unwrap(), "format-11.answers");
    assert_relationship_fixture_answers(&Database::open(&path).unwrap(), "format-11.answers");
}

#[test]
fn a_database_of_format_eleven_takes_relationship_writes_after_the_upgrade() {
    assert_takes_relationship_writes_after_the_upgrade(FORMAT_ELEVEN);
}

fn assert_takes_relationship_writes_after_the_upgrade(fixture: &str) {
    let directory = tempfile::tempdir().unwrap();
    let path = copy_of_engine_file(fixture, directory.path());
    {
        let db = Database::open(&path).unwrap();
        run(
            &db,
            "MATCH ()-[r:EVERY]->() SET r.i = r.i + 10, r.added = [true]",
        );
        run(&db, "MATCH ()-[r:MANY]->() REMOVE r.p128");
        run(&db, "MATCH ()-[r:T7]->() DELETE r");
        run(
            &db,
            "MATCH (a:Item {i: 16998}), (b:Item {i: 3}) CREATE (a)-[:NEW {w: -1}]->(b)",
        );
        run(&db, "MATCH (x:Item {i: 128}) DETACH DELETE x");
    }
    let db = Database::open(&path).unwrap();
    assert_eq!(
        run(
            &db,
            "MATCH ()-[r:EVERY]->() RETURN r.i, r.added, r.s ORDER BY r.i"
        ),
        [
            vec![
                int(11),
                Value::List(vec![Value::Boolean(true)]),
                string("text")
            ],
            vec![
                int(12),
                Value::List(vec![Value::Boolean(true)]),
                string("한국어")
            ],
        ]
    );
    assert_eq!(
        run(
            &db,
            "MATCH ()-[r:MANY]->() RETURN size(keys(r)), r.p128, r.p209"
        ),
        [vec![int(209), Value::Null, int(209)]]
    );
    assert_eq!(single(&db, "MATCH ()-[r:T7]->() RETURN count(*)"), int(0));
    assert_eq!(
        run(&db, "MATCH (a)-[r:NEW]->(b) RETURN a.i, r.w, b.i"),
        [vec![int(16998), int(-1), int(3)]]
    );
    assert_eq!(
        single(
            &db,
            "MATCH (a:Item)-[:NEXT*1..3]->(b:Item {i: 129}) RETURN count(*)"
        ),
        int(0)
    );
}
