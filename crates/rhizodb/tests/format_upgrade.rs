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

const FORMAT_FOURTEEN: &str = "tests/fixtures/format-14.db";

#[test]
fn a_database_of_format_fourteen_answers_and_passes_the_check_after_the_upgrade() {
    let directory = tempfile::tempdir().unwrap();
    let path = copy_of_engine_file(FORMAT_FOURTEEN, directory.path());
    for _ in 0..2 {
        let db = Database::open(&path).unwrap();
        assert_relationship_fixture_answers(&db, "format-14.answers");
        db.check().unwrap();
    }
}

#[test]
fn a_database_of_format_fourteen_takes_relationship_writes_after_the_upgrade() {
    assert_takes_relationship_writes_after_the_upgrade(FORMAT_FOURTEEN);
}

const FORMAT_FIFTEEN: &str = "tests/fixtures/format-15.db";

#[test]
fn a_database_of_format_fifteen_answers_and_passes_the_check_after_the_upgrade() {
    let directory = tempfile::tempdir().unwrap();
    let path = copy_of_engine_file(FORMAT_FIFTEEN, directory.path());
    for _ in 0..2 {
        let db = Database::open(&path).unwrap();
        assert_relationship_fixture_answers(&db, "format-15.answers");
        db.check().unwrap();
    }
}

#[test]
fn a_database_of_format_fifteen_takes_relationship_writes_after_the_upgrade() {
    assert_takes_relationship_writes_after_the_upgrade(FORMAT_FIFTEEN);
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

/// The databases that release 0.1.0 wrote, one closed and one aborted after its last commit.
const RELEASE_ZERO_ONE: [&str; 2] = ["release-0-1-0-clean", "release-0-1-0-crashed"];

/// Copies the database file of a release fixture and its log segments, under their own names,
/// into `directory`, and returns the path of the database file.
fn copy_of_release_fixture(fixture: &str, directory: &Path) -> PathBuf {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let file_name = format!("{fixture}.rhizodb");
    for entry in std::fs::read_dir(&fixtures).unwrap() {
        let name = entry.unwrap().file_name();
        if name.to_str().unwrap().starts_with(&file_name) {
            std::fs::copy(fixtures.join(&name), directory.join(&name)).unwrap();
        }
    }
    directory.join(file_name)
}

fn release_dump(fixture: &str) -> String {
    std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(format!("{fixture}.dump")),
    )
    .unwrap()
}

fn exported(db: &Database) -> String {
    let mut out = Vec::new();
    db.export(&mut out).unwrap();
    String::from_utf8(out).unwrap()
}

#[test]
fn the_crashed_release_fixture_needs_its_log_and_replays_it_after_the_upgrade() {
    let directory = tempfile::tempdir().unwrap();
    let path = copy_of_release_fixture("release-0-1-0-crashed", directory.path());
    let log = directory
        .path()
        .join("release-0-1-0-crashed.rhizodb-wal-0000000000000001");
    let kept = std::fs::read(&log).unwrap();
    std::fs::remove_file(&log).unwrap();
    assert_eq!(
        Database::open(&path).err().map(|error| error.kind()),
        Some(rhizodb::ErrorKind::CorruptData),
        "the file was left open, so it refuses to open without its log"
    );
    std::fs::write(&log, kept).unwrap();
    assert_eq!(
        single(
            &Database::open(&path).unwrap(),
            "MATCH (p:Late) RETURN count(*)"
        ),
        int(9),
        "the commits after the last checkpoint replay from the log"
    );
}

#[test]
fn a_database_of_release_zero_one_keeps_every_element_and_index_answer_after_the_upgrade() {
    for fixture in RELEASE_ZERO_ONE {
        let directory = tempfile::tempdir().unwrap();
        let path = copy_of_release_fixture(fixture, directory.path());
        let dump = release_dump(fixture);
        let answers = answers_in(&format!("{fixture}.answers"));
        assert_eq!(answers.len(), 14);
        for _ in 0..2 {
            let db = Database::open(&path).unwrap();
            db.check().unwrap();
            let upgraded = exported(&db);
            for (at, (line, expected)) in upgraded.lines().zip(dump.lines()).enumerate() {
                assert_eq!(line, expected, "{fixture}: line {} of the dump", at + 1);
            }
            assert_eq!(upgraded.len(), dump.len(), "{fixture}");
            for (query, expected) in &answers {
                assert_eq!(
                    format!("{:?}", run(&db, query)),
                    *expected,
                    "{fixture}: {query}"
                );
            }
        }
    }
}

#[test]
fn a_database_of_release_zero_one_takes_writes_after_the_upgrade_and_exports_dump_version_one() {
    for fixture in RELEASE_ZERO_ONE {
        let directory = tempfile::tempdir().unwrap();
        let path = copy_of_release_fixture(fixture, directory.path());
        {
            let db = Database::open(&path).unwrap();
            run(&db, "MATCH (p:Person {name: 'person 17'}) SET p.age = 99");
            run(
                &db,
                "CREATE (:Person {name: 'person 200', age: 31})-[:KNOWS {since: 2030, note: 'conference again', weight: [3.0, 4.0]}]->(:Doc {i: 200, title: 'doc 200', text: 'graph quokka', vec: [1.0, 0.5, 0.25]})",
            );
            run(&db, "MATCH (d:Doc) WHERE d.i % 8 = 1 DETACH DELETE d");
            run(&db, "MATCH ()-[k:KNOWS]->() WHERE k.since % 5 = 0 DELETE k");
            run(&db, "MATCH (v:Values) SET v.again = v.zoned_datetime");
        }
        let db = Database::open(&path).unwrap();
        db.check().unwrap();
        assert_eq!(
            run(
                &db,
                "MATCH (p:Person) WHERE p.age = 99 OR p.name = 'person 200' RETURN p.name ORDER BY p.name"
            ),
            [vec![string("person 17")], vec![string("person 200")]]
        );
        assert_eq!(
            single(
                &db,
                "CALL db.index.keyword.queryNodes('doc text', 'quokka', 10) YIELD node RETURN node.i"
            ),
            int(200)
        );
        assert_eq!(
            single(&db, "MATCH (v:Values) RETURN v.again = v.zoned_datetime"),
            Value::Boolean(true)
        );
        db.create_keyword_index(
            "doc text again",
            rhizodb::IndexTarget::Label("Doc"),
            &["title", "text"],
        )
        .unwrap();
        db.create_keyword_index(
            "knows note again",
            rhizodb::IndexTarget::RelationshipType("KNOWS"),
            &["note"],
        )
        .unwrap();
        db.create_vector_index(
            "doc vec again",
            rhizodb::IndexTarget::Label("Doc"),
            "vec",
            3,
            rhizodb::VectorMetric::Cosine,
        )
        .unwrap();
        db.create_vector_index(
            "knows weight again",
            rhizodb::IndexTarget::RelationshipType("KNOWS"),
            "weight",
            2,
            rhizodb::VectorMetric::DotProduct,
        )
        .unwrap();
        for search in [
            "CALL db.index.keyword.queryNodes('doc text{again}', 'graph', 1000) YIELD node, score RETURN id(node), score",
            "CALL db.index.keyword.queryNodes('doc text{again}', 'stor*', 1000) YIELD node, score RETURN id(node), score",
            "CALL db.index.keyword.queryRelationships('knows note{again}', 'conference', 1000) YIELD relationship, score RETURN id(relationship), score",
            "CALL db.index.vector.queryNodes('doc vec{again}', [1.0, 0.5, 0.25], 1000) YIELD node, score RETURN id(node), score",
            "CALL db.index.vector.queryRelationships('knows weight{again}', [1.0, 2.0], 1000) YIELD relationship, score RETURN id(relationship), score",
        ] {
            let upgraded = run(&db, &search.replace("{again}", ""));
            assert!(!upgraded.is_empty(), "{fixture}: {search}");
            assert_eq!(
                upgraded,
                run(&db, &search.replace("{again}", " again")),
                "{fixture}: {search}"
            );
        }
        let dump = exported(&db);
        let header = dump.lines().next().unwrap();
        assert!(
            header.starts_with("{\"type\":\"header\",\"format\":\"rhizodb-dump\",\"version\":1,"),
            "{header}"
        );
    }
}

/// The engine format `(major, minor)` that each of the two header slots at the start of a
/// database file records, as little-endian `u16`s after the eight-byte magic.
fn engine_formats(path: &Path) -> Vec<(u16, u16)> {
    let bytes = std::fs::read(path).unwrap();
    bytes
        .chunks(4096)
        .take(2)
        .map(|slot| {
            assert_eq!(&slot[..8], b"RHIZODB\0");
            (
                u16::from_le_bytes([slot[8], slot[9]]),
                u16::from_le_bytes([slot[10], slot[11]]),
            )
        })
        .collect()
}

/// Whether release 0.1.0 opens a file of engine format `(major, minor)`. Its header decoder
/// writes 1.3, reads 1.0 to 1.3, and refuses another major or a newer minor.
fn release_zero_one_reads((major, minor): (u16, u16)) -> bool {
    major == 1 && minor <= 3
}

/// Each file of a release fixture, by name, with its bytes.
fn release_fixture_files(fixture: &str) -> Vec<(String, Vec<u8>)> {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut files: Vec<_> = std::fs::read_dir(&fixtures)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.starts_with(&format!("{fixture}.rhizodb")))
        .map(|name| {
            let bytes = std::fs::read(fixtures.join(&name)).unwrap();
            (name, bytes)
        })
        .collect();
    files.sort();
    files
}

#[test]
fn a_database_of_release_zero_one_moves_to_engine_minor_four_and_the_current_graph_format() {
    for fixture in RELEASE_ZERO_ONE {
        let originals = release_fixture_files(fixture);
        let directory = tempfile::tempdir().unwrap();
        let path = copy_of_release_fixture(fixture, directory.path());
        let formats = engine_formats(&path);
        assert_eq!(formats, [(1, 3), (1, 3)], "{fixture}");
        assert!(
            formats.iter().all(|format| release_zero_one_reads(*format)),
            "{fixture}"
        );

        let db = Database::open(&path).unwrap();
        db.check().unwrap();
        drop(db);
        let formats = engine_formats(&path);
        assert_eq!(formats, [(1, 4), (1, 4)], "{fixture}");
        assert!(
            formats
                .iter()
                .all(|format| !release_zero_one_reads(*format)),
            "{fixture}: release 0.1.0 refuses the upgraded file"
        );
        // The writer open above also moved the graph to this build's format. A replica opens
        // without writing and serves no graph reads until its format is current, so a replica
        // of the upgraded file reads it only because that upgrade ran.
        let db = Database::open(&path).unwrap();
        let snapshot = db.snapshot().unwrap();
        let mut stream = vec![0; usize::try_from(snapshot.len()).unwrap()];
        let mut offset = 0;
        while offset < stream.len() {
            offset += snapshot
                .read_at(offset as u64, &mut stream[offset..])
                .unwrap();
        }
        let replica =
            Database::install_snapshot(directory.path().join("replica.rhizodb"), stream.as_slice())
                .unwrap();
        assert_eq!(
            single(&replica, "MATCH (n) RETURN count(n)"),
            single(&db, "MATCH (n) RETURN count(n)"),
            "{fixture}"
        );
        drop(snapshot);
        drop(db);
        assert_eq!(engine_formats(&path), [(1, 4), (1, 4)], "{fixture}");

        let original = &originals
            .iter()
            .find(|(name, _)| path.ends_with(name))
            .unwrap()
            .1;
        assert_ne!(&std::fs::read(&path).unwrap(), original, "{fixture}");
        assert_eq!(release_fixture_files(fixture), originals, "{fixture}");
    }
}
