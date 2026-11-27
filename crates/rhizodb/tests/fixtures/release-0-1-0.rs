//! Writes the release 0.1.0 fixtures through the public API of the `v0.1.0` tag.
//! `generate-release-0-1-0.sh` builds it against that tag. Run as
//! `release-0-1-0 <clean|crashed> <database path> <dump path>`, it writes the database, exports
//! it to the dump path, and prints each query the upgrade test checks, each followed by its
//! answer on an indented line, which the script keeps in an answers file. In `clean` mode it then
//! closes the database. In `crashed` mode it aborts the process instead, so the file keeps the
//! writes after its last checkpoint only in its write-ahead log. Run as
//! `release-0-1-0 import <database path> <dump path>`, it imports the dump into a new database,
//! checks it, and exports it again to standard output, which shows that 0.1.0 reads a dump that a
//! later build wrote.

use rhizodb::Database;
use rhizodb::Date;
use rhizodb::DateTime;
use rhizodb::Duration;
use rhizodb::IndexTarget;
use rhizodb::LocalDateTime;
use rhizodb::LocalTime;
use rhizodb::Parameters;
use rhizodb::Temporal;
use rhizodb::Time;
use rhizodb::Value;
use rhizodb::VectorMetric;

const QUERIES: &[&str] = &[
    "MATCH (n) RETURN count(*), min(id(n)), max(id(n))",
    "MATCH ()-[r]->() RETURN count(*), min(id(r)), max(id(r))",
    "MATCH (p:Person {name: 'person 17'}) RETURN id(p), p.age",
    "MATCH (p:Person) WHERE p.age >= 30 AND p.age < 33 RETURN p.name, p.age ORDER BY p.name",
    "MATCH (p:Person) WHERE p.age IN [20, 44, 59] RETURN count(*), sum(p.age)",
    "MATCH ()-[k:KNOWS]->() WHERE k.since > 2015 RETURN count(*), sum(k.since)",
    "CALL db.index.keyword.queryNodes('doc text', 'graph', 10) YIELD node, score RETURN id(node), node.i, score",
    "CALL db.index.keyword.queryNodes('doc text', 'stor*', 1000) YIELD node RETURN count(*), sum(node.i)",
    "CALL db.index.keyword.queryNodes('doc text', '\"river bridge\"', 10) YIELD node, score RETURN node.i, score",
    "CALL db.index.keyword.queryNodes('doc text', '그래프', 10) YIELD node, score RETURN node.i, score",
    "CALL db.index.keyword.queryNodes('doc text', 'gone', 10) YIELD node RETURN count(*)",
    "CALL db.index.keyword.queryRelationships('knows note', 'conference', 10) YIELD relationship, score RETURN id(relationship), score",
    "CALL db.index.vector.queryNodes('doc vec', [1.0, 0.5, 0.25], 5) YIELD node, score RETURN node.i, score",
    "CALL db.index.vector.queryRelationships('knows weight', [1.0, 2.0], 5) YIELD relationship, score RETURN id(relationship), score",
];

const WORDS: &[&str] = &[
    "graph", "store", "river", "bridge", "storage", "ocean", "market", "city", "summer",
];

fn parameters(entries: Vec<(&str, Value)>) -> Parameters {
    entries
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value))
        .collect()
}

fn exec(db: &Database, query: &str) {
    db.execute(query, &Parameters::new()).unwrap();
}

fn temporal(value: Temporal) -> Value {
    Value::Temporal(value)
}

/// One value of every type a property holds, with the edge cases of each.
fn every_value() -> Value {
    let date = Date::from_ymd(-44, 3, 15).unwrap();
    let late = LocalTime::from_hms_nano(23, 59, 59, 999_999_999).unwrap();
    let landing = LocalDateTime::new(
        Date::from_ymd(1969, 7, 20).unwrap(),
        LocalTime::from_hms_nano(20, 17, 40, 0).unwrap(),
    );
    let berlin = LocalDateTime::new(
        Date::from_ymd(2024, 10, 27).unwrap(),
        LocalTime::from_hms_nano(2, 30, 0, 0).unwrap(),
    );
    let entries: Vec<(&str, Value)> = vec![
        ("yes", Value::Boolean(true)),
        ("no", Value::Boolean(false)),
        ("zero", Value::Integer(0)),
        ("min", Value::Integer(i64::MIN)),
        ("max", Value::Integer(i64::MAX)),
        ("one", Value::Float(1.0)),
        ("tenth", Value::Float(0.1)),
        ("negzero", Value::Float(-0.0)),
        ("tiny", Value::Float(f64::from_bits(1))),
        ("inf", Value::Float(f64::INFINITY)),
        ("neginf", Value::Float(f64::NEG_INFINITY)),
        ("nan", Value::Float(f64::from_bits(0x7ff8_0000_0000_0001))),
        ("empty", Value::String(String::new())),
        ("text", Value::String("\"quoted\"\n\u{1}\t그래프 图 🦀".to_owned())),
        ("long", Value::String("long text ".repeat(400))),
        ("bytes", Value::Bytes(vec![0, 0xff, 0x10, 0x80])),
        ("nobytes", Value::Bytes(Vec::new())),
        (
            "ints",
            Value::List((-3..4).map(Value::Integer).collect()),
        ),
        (
            "floats",
            Value::List(vec![Value::Float(0.5), Value::Float(-0.0), Value::Float(2.0)]),
        ),
        (
            "strings",
            Value::List(vec![Value::String("a".to_owned()), Value::String(String::new())]),
        ),
        (
            "mixed",
            Value::List(vec![
                Value::List(vec![Value::Integer(1), Value::Float(2.0)]),
                Value::List(vec![Value::String("x".to_owned()), Value::Boolean(true)]),
                Value::List(Vec::new()),
                Value::Bytes(vec![7]),
                temporal(Temporal::Date(date)),
            ]),
        ),
        ("nothing", Value::List(Vec::new())),
        ("date", temporal(Temporal::Date(date))),
        (
            "bigdate",
            temporal(Temporal::Date(Date::from_ymd(12345, 1, 1).unwrap())),
        ),
        ("localtime", temporal(Temporal::LocalTime(late))),
        (
            "time",
            temporal(Temporal::Time(
                Time::new(LocalTime::from_hms_nano(12, 0, 0, 1).unwrap(), -19_800).unwrap(),
            )),
        ),
        ("localdatetime", temporal(Temporal::LocalDateTime(landing))),
        (
            "offset_datetime",
            temporal(Temporal::DateTime(DateTime::with_offset(landing, 0).unwrap())),
        ),
        (
            "zoned_datetime",
            temporal(Temporal::DateTime(
                DateTime::in_zone(berlin, "Europe/Berlin").unwrap(),
            )),
        ),
        (
            "duration",
            temporal(Temporal::Duration(
                Duration::new(14, -3, -6, 999_999_993).unwrap(),
            )),
        ),
    ];
    Value::Map(
        entries
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value))
            .collect(),
    )
}

fn text(i: i64) -> String {
    let words: Vec<&str> = (0..(2 + i % 5))
        .map(|at| WORDS[((i * 5 + at * at + at) % WORDS.len() as i64) as usize])
        .collect();
    let mut text = words.join(" ");
    if i % 7 == 0 {
        text.push_str(" 그래프 데이터베이스");
    }
    text
}

/// Writes that a checkpoint takes into the file before the second phase.
fn first_phase(db: &Database) {
    db.create_property_index("person name", IndexTarget::Label("Person"), "name")
        .unwrap();
    db.create_property_index("person age", IndexTarget::Label("Person"), "age")
        .unwrap();
    db.create_keyword_index("doc text", IndexTarget::Label("Doc"), &["title", "text"])
        .unwrap();
    db.create_vector_index(
        "doc vec",
        IndexTarget::Label("Doc"),
        "vec",
        3,
        VectorMetric::Cosine,
    )
    .unwrap();
    db.create_keyword_index("dropped", IndexTarget::Label("Doc"), &["text"])
        .unwrap();
    exec(
        db,
        "UNWIND range(0, 59) AS i CREATE (:Person {name: 'person ' + toString(i), age: 20 + i % 40})",
    );
    let docs = (0..80)
        .map(|i| {
            Value::Map(
                [
                    ("i".to_owned(), Value::Integer(i)),
                    ("text".to_owned(), Value::String(text(i))),
                    (
                        "vec".to_owned(),
                        Value::List(vec![
                            Value::Float(1.0 + (i % 5) as f64),
                            Value::Float((i % 3) as f64 * 0.5),
                            Value::Float(0.25 * (i % 7) as f64),
                        ]),
                    ),
                ]
                .into(),
            )
        })
        .collect();
    db.execute(
        "UNWIND $docs AS doc CREATE (:Doc {i: doc.i, title: 'doc ' + toString(doc.i), text: doc.text, vec: doc.vec})",
        &parameters(vec![("docs", Value::List(docs))]),
    )
    .unwrap();
    exec(
        db,
        "MATCH (a:Person), (b:Person) WHERE b.age = a.age + 1 AND id(a) % 3 = 0 \
         CREATE (a)-[:KNOWS {since: 2000 + id(a) % 25, note: CASE WHEN id(a) % 2 = 0 THEN 'met at a conference' ELSE 'old neighbours' END}]->(b)",
    );
    exec(
        db,
        "MATCH (p:Person), (d:Doc) WHERE d.i = id(p) % 80 CREATE (p)-[:WROTE {at: id(p)}]->(d)",
    );
    db.execute(
        "CREATE (v:Values:Every) SET v = $values CREATE (v)-[:SELF]->(v) \
         WITH v MATCH (p:Person {name: 'person 0'}) CREATE (p)-[r:HOLDS]->(v) SET r = $values",
        &parameters(vec![("values", every_value())]),
    )
    .unwrap();
    exec(db, "MATCH (d:Doc) WHERE d.i % 11 = 0 SET d.text = 'gone soon'");
    exec(db, "MATCH (d:Doc) WHERE d.i % 11 = 0 DETACH DELETE d");
    exec(db, "MATCH (p:Person) WHERE id(p) % 13 = 5 DETACH DELETE p");
}

/// Writes after the checkpoint, which the crashed fixture keeps only in its log.
fn second_phase(db: &Database) {
    db.create_keyword_index(
        "knows note",
        IndexTarget::RelationshipType("KNOWS"),
        &["note"],
    )
    .unwrap();
    db.create_vector_index(
        "knows weight",
        IndexTarget::RelationshipType("KNOWS"),
        "weight",
        2,
        VectorMetric::DotProduct,
    )
    .unwrap();
    assert!(db.drop_index("dropped").unwrap());
    exec(
        db,
        "MATCH ()-[k:KNOWS]->() SET k.weight = [toFloat(k.since % 7), toFloat(k.since % 3)]",
    );
    exec(db, "MATCH ()-[k:KNOWS]->() WHERE k.since % 4 = 1 DELETE k");
    exec(
        db,
        "MATCH (d:Doc) WHERE d.i % 9 = 2 SET d.text = 'storage rewritten river bridge', d.extra = [1, 2]",
    );
    exec(db, "MATCH (d:Doc) WHERE d.i % 10 = 3 REMOVE d.vec, d.title");
    exec(db, "MATCH (p:Person) WHERE p.age = 21 REMOVE p:Person SET p:Former");
    exec(
        db,
        "UNWIND range(60, 69) AS i CREATE (:Person:Late {name: 'person ' + toString(i), age: i - 30})",
    );
    exec(db, "MATCH (v:Values) REMOVE v.empty SET v.added = date('2025-01-31')");
    exec(db, "MATCH (p:Person {name: 'person 61'}) DETACH DELETE p");
}

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(mode), Some(path), Some(dump)) = (args.next(), args.next(), args.next()) else {
        panic!("usage: release-0-1-0 <clean|crashed|import> <database path> <dump path>");
    };
    let db = Database::open(&path).unwrap();
    if mode == "import" {
        db.import(std::io::BufReader::new(std::fs::File::open(&dump).unwrap()))
            .unwrap();
        db.check().unwrap();
        db.export(std::io::stdout().lock()).unwrap();
        return;
    }
    first_phase(&db);
    db.compact().unwrap();
    second_phase(&db);
    let mut out = Vec::new();
    db.export(&mut out).unwrap();
    std::fs::write(&dump, out).unwrap();
    for query in QUERIES {
        let result = db.execute(query, &Parameters::new()).unwrap();
        println!("{query}\n  {:?}", result.rows());
    }
    match mode.as_str() {
        "clean" => drop(db),
        "crashed" => {
            use std::io::Write;
            std::io::stdout().flush().unwrap();
            std::process::abort();
        }
        other => panic!("unknown mode {other}"),
    }
}
