//! Imports the checked-in dump of format version 1 and checks every value and index it holds,
//! checks that exporting it again gives the same bytes, and checks that every malformed or cut
//! dump fails with its own error kind.

mod support;

use std::collections::BTreeMap;
use std::io;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use rhizodb::Database;
use rhizodb::Date;
use rhizodb::DateTime;
use rhizodb::Duration;
use rhizodb::ErrorKind;
use rhizodb::IndexTarget;
use rhizodb::LocalDateTime;
use rhizodb::LocalTime;
use rhizodb::Parameters;
use rhizodb::Temporal;
use rhizodb::Time;
use rhizodb::Value;
use support::*;

const FIXTURE: &str = include_str!("fixtures/dump-v1.jsonl");

fn imported(text: &str) -> Database {
    let db = db();
    db.import(text.as_bytes()).unwrap();
    db
}

fn exported(db: &Database) -> String {
    let mut out = Vec::new();
    db.export(&mut out).unwrap();
    String::from_utf8(out).unwrap()
}

fn import_error(text: &str) -> ErrorKind {
    let db = db();
    match db.import(text.as_bytes()) {
        Ok(()) => panic!("imported {text:?}"),
        Err(error) => error.kind(),
    }
}

fn properties(db: &Database, id: i64) -> BTreeMap<String, Value> {
    let parameters = Parameters::from([("id".to_owned(), Value::Integer(id))]);
    let rows = db
        .execute("MATCH (n) WHERE id(n) = $id RETURN n", &parameters)
        .unwrap()
        .rows()
        .to_vec();
    match rows.as_slice() {
        [row] => match row.as_slice() {
            [Value::Node(node)] => node.properties().clone(),
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

fn float_bits(value: &Value) -> u64 {
    match value {
        Value::Float(value) => value.to_bits(),
        other => panic!("{other:?} is not a float"),
    }
}

fn temporal(value: Temporal) -> Value {
    Value::Temporal(value)
}

#[test]
fn the_version_one_fixture_imports_every_value_exactly() {
    let db = imported(FIXTURE);
    let ada = properties(&db, 0);
    assert_eq!(ada["age"], Value::Integer(36));
    assert_eq!(ada["height"], Value::Float(1.7));
    assert_eq!(
        ada["nickname"],
        Value::String("\"Countess\"\n\u{1}".to_owned())
    );
    let doc = properties(&db, 2);
    assert_eq!(doc["bytes"], Value::Bytes(vec![0, 0xff, 0x10]));
    assert_eq!(float_bits(&doc["inf"]), f64::INFINITY.to_bits());
    assert_eq!(float_bits(&doc["nan"]), 0x7ff8_0000_0000_0000);
    assert_eq!(float_bits(&doc["negzero"]), (-0.0f64).to_bits());
    assert_eq!(doc["one"], Value::Float(1.0));
    let times = properties(&db, 3);
    assert_eq!(
        times["date"],
        temporal(Temporal::Date(Date::from_ymd(-44, 3, 15).unwrap()))
    );
    let berlin = match &times["datetime"] {
        Value::Temporal(Temporal::DateTime(datetime)) => datetime.clone(),
        other => panic!("{other:?}"),
    };
    assert_eq!(berlin.zone_name(), Some("Europe/Berlin"));
    assert_eq!(berlin.offset_seconds(), 3600);
    assert_eq!(berlin.epoch_second(), 1_729_992_600);
    assert_eq!(
        times["duration"],
        temporal(Temporal::Duration(
            Duration::new(14, -3, -6, 999_999_993).unwrap()
        ))
    );
    assert_eq!(
        times["local"],
        temporal(Temporal::LocalTime(
            LocalTime::from_hms_nano(23, 59, 59, 999_999_999).unwrap()
        ))
    );
    assert_eq!(
        times["localdatetime"],
        temporal(Temporal::LocalDateTime(LocalDateTime::new(
            Date::from_ymd(12345, 1, 1).unwrap(),
            LocalTime::MIDNIGHT
        )))
    );
    assert_eq!(
        times["offset_datetime"],
        temporal(Temporal::DateTime(
            DateTime::with_offset(
                LocalDateTime::new(
                    Date::from_ymd(1969, 7, 20).unwrap(),
                    LocalTime::from_hms_nano(20, 17, 40, 0).unwrap()
                ),
                0
            )
            .unwrap()
        ))
    );
    assert_eq!(
        times["time"],
        temporal(Temporal::Time(
            Time::new(LocalTime::from_hms_nano(12, 0, 0, 1).unwrap(), -19_800).unwrap()
        ))
    );
    let grace = properties(&db, 4);
    assert_eq!(grace["big"], Value::Integer(i64::MIN));
    assert_eq!(
        grace["nested"],
        Value::List(vec![
            Value::List(vec![Value::Integer(1), Value::Float(2.0)]),
            Value::List(vec![Value::String("x".to_owned()), Value::Boolean(true)]),
            Value::List(vec![]),
        ])
    );
    assert_eq!(
        run(&db, "MATCH (n) RETURN id(n), labels(n) ORDER BY id(n)"),
        [
            vec![
                Value::Integer(0),
                Value::List(vec![Value::String("Person".to_owned())])
            ],
            vec![
                Value::Integer(2),
                Value::List(vec![
                    Value::String("Doc".to_owned()),
                    Value::String("Draft".to_owned())
                ])
            ],
            vec![Value::Integer(3), Value::List(vec![])],
            vec![
                Value::Integer(4),
                Value::List(vec![Value::String("Person".to_owned())])
            ],
        ]
    );
    assert_eq!(
        run(
            &db,
            "MATCH (a)-[r]->(b) RETURN id(r), type(r), id(a), id(b), r.note ORDER BY id(r)"
        ),
        [
            vec![
                Value::Integer(0),
                Value::String("KNOWS".to_owned()),
                Value::Integer(0),
                Value::Integer(4),
                Value::String("met at a graph talk".to_owned())
            ],
            vec![
                Value::Integer(2),
                Value::String("SELF".to_owned()),
                Value::Integer(3),
                Value::Integer(3),
                Value::Null
            ],
        ]
    );
}

#[test]
fn the_version_one_fixture_keeps_its_indexes_and_next_ids() {
    let db = imported(FIXTURE);
    assert_eq!(
        run(
            &db,
            "CALL db.index.keyword.queryRelationships('notes', 'code', 10) YIELD relationship \
             RETURN id(relationship)"
        ),
        [vec![Value::Integer(0)]]
    );
    assert_eq!(
        run(
            &db,
            "CALL db.index.vector.queryNodes('embedding', [1.0, 2.0, 4.0], 10) YIELD node, score \
             RETURN id(node), score"
        ),
        [vec![Value::Integer(2), Value::Float(1.0)]]
    );
    assert_eq!(
        run(&db, "MATCH (p:Person {name: 'Grace'}) RETURN id(p)"),
        [vec![Value::Integer(4)]]
    );
    assert_eq!(
        db.create_property_index("by_name", IndexTarget::Label("Person"), "age")
            .unwrap_err()
            .kind(),
        ErrorKind::IndexAlreadyExists
    );
    assert_eq!(
        run(
            &db,
            "MATCH (a), (b) WHERE id(a) = 0 AND id(b) = 4 INSERT (c)-[r:NEW]->(a) RETURN id(c), id(r)"
        ),
        [vec![Value::Integer(5), Value::Integer(3)]]
    );
}

#[test]
fn exporting_the_fixture_again_gives_its_bytes() {
    let db = imported(FIXTURE);
    assert_eq!(exported(&db), FIXTURE);
    let crlf = FIXTURE.replace('\n', "\r\n");
    assert_eq!(exported(&imported(&crlf)), FIXTURE);
    assert_eq!(exported(&imported(FIXTURE.trim_end())), FIXTURE);
}

#[test]
fn every_cut_of_a_dump_fails_as_invalid() {
    let lines: Vec<&str> = FIXTURE.lines().collect();
    for end in 0..lines.len() {
        let cut = lines[..end].join("\n");
        assert_eq!(import_error(&cut), ErrorKind::InvalidDump, "{end} lines");
    }
    for at in (0..FIXTURE.len() - 2).filter(|at| FIXTURE.is_char_boundary(*at)) {
        let kind = import_error(&FIXTURE[..at]);
        assert_eq!(kind, ErrorKind::InvalidDump, "cut at byte {at}");
    }
}

#[test]
fn a_malformed_dump_fails_as_invalid() {
    let header = r#"{"type":"header","format":"rhizodb-dump","version":1,"next_node_id":10,"next_relationship_id":10}"#;
    let end = |nodes: u64, relationships: u64| {
        format!(r#"{{"type":"end","nodes":{nodes},"relationships":{relationships}}}"#)
    };
    let node = |id: i64| format!(r#"{{"type":"node","id":{id},"labels":[],"properties":{{}}}}"#);
    let cases = [
        String::new(),
        "\n".to_owned(),
        format!("{header}\n{header}\n{}", end(0, 0)),
        r#"{"type":"header","format":"other","version":1,"next_node_id":0,"next_relationship_id":0}"#.to_string(),
        r#"{"type":"header","format":"rhizodb-dump","version":0,"next_node_id":0,"next_relationship_id":0}"#.to_string(),
        r#"{"type":"header","format":"rhizodb-dump","version":1,"next_node_id":0}"#.to_string(),
        r#"{"type":"header","format":"rhizodb-dump","version":1,"next_node_id":0,"next_relationship_id":0,"extra":1}"#.to_string(),
        format!("{header}\n{}\n{}\n{}", node(1), node(1), end(2, 0)),
        format!("{header}\n{}\n{}\n{}", node(2), node(1), end(2, 0)),
        format!("{header}\n{}\n{}", node(10), end(1, 0)),
        format!("{header}\n{}\n{}", node(-1), end(1, 0)),
        format!("{header}\n{}\n{}", node(1), end(2, 0)),
        format!("{header}\n{}\n{}\n", node(1), end(1, 0)) + "\n",
        format!("{header}\n{}\n{}\n{}", node(1), end(1, 0), node(2)),
        format!(
            "{header}\n{}\n{}",
            r#"{"type":"node","id":1,"labels":["A","A"],"properties":{}}"#,
            end(1, 0)
        ),
        format!(
            "{header}\n{}\n{}",
            r#"{"type":"node","id":1,"labels":[],"properties":{"a":1,"a":2}}"#,
            end(1, 0)
        ),
        format!(
            "{header}\n{}\n{}",
            r#"{"type":"node","id":1,"labels":[],"properties":{"a":null}}"#,
            end(1, 0)
        ),
        format!(
            "{header}\n{}\n{}",
            r#"{"type":"node","id":1,"labels":[],"properties":{"a":{"b":1}}}"#,
            end(1, 0)
        ),
        format!(
            "{header}\n{}\n{}",
            r#"{"type":"node","id":1,"labels":[]}"#,
            end(1, 0)
        ),
        format!("{header}\n{}\n{}", r#"{"type":"edge","id":1}"#, end(0, 0)),
        format!("{header}\n{}\n{}", r#"[1,2]"#, end(0, 0)),
        format!(
            "{header}\n{}\n{}",
            r#"{"type":"relationship","id":1,"relationship_type":"T","start":1,"end":2,"properties":{}}"#,
            end(0, 1)
        ),
        format!(
            "{header}\n{}\n{}\n{}",
            r#"{"type":"relationship","id":1,"relationship_type":"T","start":1,"end":1,"properties":{}}"#,
            node(1),
            end(1, 1)
        ),
        format!(
            "{header}\n{}\n{}\n{}",
            node(1),
            r#"{"type":"index","name":"i","index":"property","label":"A","property":"p"}"#,
            end(1, 0)
        ),
        format!(
            "{header}\n{}\n{}\n{}",
            r#"{"type":"index","name":"i","index":"property","label":"A","property":"p"}"#,
            r#"{"type":"index","name":"i","index":"property","label":"B","property":"p"}"#,
            end(0, 0)
        ),
        format!(
            "{header}\n{}\n{}",
            r#"{"type":"index","name":"i","index":"vector","label":"A","property":"p","dimension":0,"metric":"cosine"}"#,
            end(0, 0)
        ),
        format!(
            "{header}\n{}\n{}",
            r#"{"type":"index","name":"i","index":"vector","label":"A","property":"p","dimension":2,"metric":"hamming"}"#,
            end(0, 0)
        ),
        format!(
            "{header}\n{}\n{}",
            r#"{"type":"index","name":"i","index":"keyword","label":"A","relationship_type":"T","properties":["p"]}"#,
            end(0, 0)
        ),
        format!(
            "{header}\n{}\n{}",
            r#"{"type":"index","name":"i","index":"keyword","label":"A","properties":[]}"#,
            end(0, 0)
        ),
        format!(
            "{header}\n{}\n{}",
            r#"{"type":"index","name":"","index":"property","label":"A","property":"p"}"#,
            end(0, 0)
        ),
        format!("{header}\n\u{feff}{}", end(0, 0)),
    ];
    for case in cases {
        assert_eq!(import_error(&case), ErrorKind::InvalidDump, "{case}");
    }
    let mut bytes = format!("{header}\n").into_bytes();
    bytes.extend_from_slice(b"{\"type\":\"end\",\"nodes\":0,\"relationships\":0,\"x\":\"\xff\"}");
    let db = db();
    assert_eq!(
        db.import(bytes.as_slice()).unwrap_err().kind(),
        ErrorKind::InvalidDump
    );
}

#[test]
fn a_newer_version_fails_with_its_own_kind() {
    let newer = FIXTURE.replacen("\"version\":1", "\"version\":2", 1);
    assert_eq!(import_error(&newer), ErrorKind::UnsupportedDumpVersion);
}

#[test]
fn only_a_new_database_takes_an_import() {
    let used = db();
    run(&used, "INSERT (n) WITH n DELETE n");
    assert_eq!(
        used.import(FIXTURE.as_bytes()).unwrap_err().kind(),
        ErrorKind::DatabaseNotEmpty
    );
    let indexed = db();
    indexed
        .create_property_index("i", IndexTarget::Label("A"), "p")
        .unwrap();
    assert_eq!(
        indexed.import(FIXTURE.as_bytes()).unwrap_err().kind(),
        ErrorKind::DatabaseNotEmpty
    );
    let twice = imported(FIXTURE);
    assert_eq!(
        twice.import(FIXTURE.as_bytes()).unwrap_err().kind(),
        ErrorKind::DatabaseNotEmpty
    );
}

struct Failing {
    left: usize,
}

impl io::Write for Failing {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if self.left == 0 {
            return Err(io::Error::other("disk full"));
        }
        let taken = buffer.len().min(self.left);
        self.left -= taken;
        Ok(taken)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl io::Read for Failing {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let at = FIXTURE.len() - self.left;
        if self.left < FIXTURE.len() / 2 {
            return Err(io::Error::other("connection reset"));
        }
        let taken = buffer.len().min(self.left).min(7);
        buffer[..taken].copy_from_slice(&FIXTURE.as_bytes()[at..at + taken]);
        self.left -= taken;
        Ok(taken)
    }
}

#[test]
fn a_failing_reader_or_writer_fails_as_io() {
    let db = imported(FIXTURE);
    for left in [0, 1, 100, FIXTURE.len() - 1] {
        assert_eq!(
            db.export(Failing { left }).unwrap_err().kind(),
            ErrorKind::Io
        );
    }
    let copy = support::db();
    let reader = io::BufReader::new(Failing {
        left: FIXTURE.len(),
    });
    assert_eq!(copy.import(reader).unwrap_err().kind(), ErrorKind::Io);
}

#[test]
fn a_dump_larger_than_one_batch_imports_whole() {
    let db = db();
    db.create_keyword_index("words", IndexTarget::Label("Item"), &["text"])
        .unwrap();
    db.create_property_index("item_i", IndexTarget::Label("Item"), "i")
        .unwrap();
    run(
        &db,
        "UNWIND range(0, 24999) AS i INSERT (:Item {i: i, text: 'word ' + toString(i % 10)})",
    );
    run(
        &db,
        "MATCH (a:Item) WHERE a.i % 2 = 0 MATCH (b:Item {i: a.i + 1}) INSERT (a)-[:NEXT]->(b)",
    );
    let dump = exported(&db);
    let copy = imported(&dump);
    assert_eq!(exported(&copy), dump);
    assert_eq!(
        single(
            &copy,
            "CALL db.index.keyword.queryNodes('words', 'word', 100000) YIELD node RETURN count(node)"
        ),
        Value::Integer(25_000)
    );
    assert_eq!(
        single(&copy, "MATCH ()-[r:NEXT]->() RETURN count(r)"),
        Value::Integer(12_500)
    );
}

#[test]
fn an_export_while_a_writer_commits_holds_one_commit_whole() {
    let db = Arc::new(db());
    run(&db, "INSERT (:Counter {n: 0})");
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let db = Arc::clone(&db);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut i = 0;
            while !stop.load(Ordering::Relaxed) || i < 20 {
                i += 1;
                let parameters = Parameters::from([("i".to_owned(), Value::Integer(i))]);
                let mut tx = db.begin_write().unwrap();
                tx.execute(
                    "UNWIND range(1, 50) AS j INSERT (:Tick {i: $i})",
                    &parameters,
                )
                .unwrap();
                tx.execute("MATCH (c:Counter) SET c.n = $i", &parameters)
                    .unwrap();
                tx.commit().unwrap();
            }
        })
    };
    for _ in 0..10 {
        let copy = imported(&exported(&db));
        let n = match single(&copy, "MATCH (c:Counter) RETURN c.n") {
            Value::Integer(n) => n,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            single(&copy, "MATCH (t:Tick) RETURN count(t)"),
            Value::Integer(n * 50)
        );
    }
    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();
}
