use rhizodb::Database;
use rhizodb::ErrorKind;
use rhizodb::IndexTarget;
use rhizodb::OpenOptions;
use rhizodb::Parameters;
use rhizodb::Position;

fn replica_at(path: &std::path::Path) -> Database {
    OpenOptions::new().replica(true).open(path).unwrap()
}

#[test]
fn a_replica_refuses_every_local_write_and_serves_reads() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("graph.rhizodb");
    let writer = Database::open(&path).unwrap();
    writer
        .execute("INSERT (:A {v: 1})", &Parameters::new())
        .unwrap();
    drop(writer);
    let replica = replica_at(&path);
    let rows = replica
        .execute("MATCH (n:A) RETURN n.v", &Parameters::new())
        .unwrap();
    assert_eq!(rows.rows().len(), 1);
    let refusals = [
        replica
            .execute("INSERT (:B)", &Parameters::new())
            .unwrap_err(),
        replica.begin_write().unwrap_err(),
        replica
            .create_property_index("by_v", IndexTarget::Label("A"), "v")
            .unwrap_err(),
        replica.drop_index("missing").unwrap_err(),
        replica.import(&b""[..]).unwrap_err(),
    ];
    for error in refusals {
        assert_eq!(error.kind(), ErrorKind::ReplicaReadOnly, "{error}");
    }
    replica.begin_read().unwrap();
    replica.check().unwrap();
}

#[test]
fn a_replica_creates_a_missing_file_that_a_writer_then_opens() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("graph.rhizodb");
    let replica = replica_at(&path);
    replica.check().unwrap();
    drop(replica);
    let writer = Database::open(&path).unwrap();
    writer.execute("INSERT (:A)", &Parameters::new()).unwrap();
    drop(writer);
    let replica = replica_at(&path);
    let rows = replica
        .execute("MATCH (n:A) RETURN count(n)", &Parameters::new())
        .unwrap();
    assert_eq!(rows.rows(), [vec![rhizodb::Value::Integer(1)]]);
}

fn sync(from: &Database, to: &Database) -> rhizodb::Position {
    loop {
        let changes = from.changes_since(&to.position(), 4096).unwrap();
        if changes.is_empty() {
            return to.position();
        }
        let received = rhizodb::Changes::from_bytes(changes.as_bytes()).unwrap();
        to.apply(&received).unwrap();
    }
}

fn dump(db: &Database) -> Vec<u8> {
    let mut out = Vec::new();
    db.export(&mut out).unwrap();
    out
}

fn in_memory_replica() -> Database {
    OpenOptions::new().replica(true).in_memory().unwrap()
}

#[test]
fn a_replica_reaches_its_writer_and_holds_the_same_graph() {
    let writer = Database::in_memory().unwrap();
    writer.promote(1, b"a").unwrap();
    for i in 0..50 {
        let mut parameters = Parameters::new();
        parameters.insert("i", i);
        writer
            .execute("INSERT (:N {i: $i})-[:R]->(:M)", &parameters)
            .unwrap();
    }
    writer
        .create_property_index("by_i", IndexTarget::Label("N"), "i")
        .unwrap();
    let replica = in_memory_replica();
    assert_eq!(sync(&writer, &replica), writer.position());
    assert_eq!(dump(&replica), dump(&writer));
    let rows = replica
        .execute("MATCH (n:N) WHERE n.i = 7 RETURN n.i", &Parameters::new())
        .unwrap();
    assert_eq!(rows.rows().len(), 1);
}

#[test]
fn changes_relay_from_one_replica_to_the_next() {
    let writer = Database::in_memory().unwrap();
    writer.promote(1, b"a").unwrap();
    writer.execute("INSERT (:A)", &Parameters::new()).unwrap();
    let b = in_memory_replica();
    let c = in_memory_replica();
    sync(&writer, &b);
    sync(&b, &c);
    assert_eq!(c.position(), writer.position());
    assert_eq!(dump(&c), dump(&writer));
}

#[test]
fn a_planned_handover_loses_no_write_and_fences_the_old_writer() {
    let a = Database::in_memory().unwrap();
    a.promote(1, b"a").unwrap();
    a.execute("INSERT (:Before)", &Parameters::new()).unwrap();
    let b = in_memory_replica();
    sync(&a, &b);
    a.execute("INSERT (:Late)", &Parameters::new()).unwrap();
    let last = a.demote().unwrap();
    assert_eq!(
        a.execute("INSERT ()", &Parameters::new())
            .unwrap_err()
            .kind(),
        ErrorKind::ReplicaReadOnly
    );
    assert_eq!(sync(&a, &b), last);
    b.promote(2, b"b").unwrap();
    b.execute("INSERT (:After)", &Parameters::new()).unwrap();
    sync(&b, &a);
    assert_eq!(dump(&a), dump(&b));
    assert_eq!(a.promote(1, b"a").unwrap_err().kind(), ErrorKind::Fenced);
    assert_eq!(a.promote(2, b"x").unwrap_err().kind(), ErrorKind::Diverged);
}

#[test]
fn two_writers_of_one_term_diverge_and_neither_is_chosen() {
    let origin = Database::in_memory().unwrap();
    origin
        .execute("INSERT (:Shared)", &Parameters::new())
        .unwrap();
    let a = in_memory_replica();
    let b = in_memory_replica();
    sync(&origin, &a);
    sync(&origin, &b);
    a.promote(5, b"a").unwrap();
    b.promote(5, b"b").unwrap();
    a.execute("INSERT (:A)", &Parameters::new()).unwrap();
    let error = b.changes_since(&a.position(), 1 << 20).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Diverged);
    let error = a.changes_since(&b.position(), 1 << 20).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Diverged);
    let c = in_memory_replica();
    sync(&a, &c);
    let from_b = b.changes_since(&origin.position(), 1 << 20).unwrap();
    assert_eq!(c.apply(&from_b).unwrap_err().kind(), ErrorKind::Diverged);
}

#[test]
fn changes_of_a_stale_writer_are_fenced() {
    let a = Database::in_memory().unwrap();
    a.promote(1, b"a").unwrap();
    let b = in_memory_replica();
    sync(&a, &b);
    b.promote(2, b"b").unwrap();
    b.demote().unwrap();
    a.execute("INSERT (:Stale)", &Parameters::new()).unwrap();
    let stale = a.changes_since(&Position::start(), 1 << 20).unwrap();
    assert_eq!(b.apply(&stale).unwrap_err().kind(), ErrorKind::Fenced);
}

#[test]
fn a_writer_refuses_changes_and_gaps_are_refused() {
    let a = Database::in_memory().unwrap();
    a.execute("INSERT (:One)", &Parameters::new()).unwrap();
    let held = a.position();
    a.execute("INSERT (:Two)", &Parameters::new()).unwrap();
    let later = a.changes_since(&held, 1 << 20).unwrap();
    let other = Database::in_memory().unwrap();
    assert_eq!(
        other.apply(&later).unwrap_err().kind(),
        ErrorKind::NotReplica
    );
    let fresh = in_memory_replica();
    assert_eq!(fresh.apply(&later).unwrap_err().kind(), ErrorKind::Gap);
    let all = a.changes_since(&Position::start(), 1 << 20).unwrap();
    fresh.apply(&all).unwrap();
    fresh.apply(&all).unwrap();
    assert_eq!(fresh.position(), a.position());
}

#[test]
fn a_replica_reads_with_the_indexes_its_writer_creates_and_drops_later() {
    let writer = Database::in_memory().unwrap();
    writer
        .execute(
            "INSERT (:Doc {text: 'replicated words'})",
            &Parameters::new(),
        )
        .unwrap();
    let replica = in_memory_replica();
    sync(&writer, &replica);
    let search = "CALL db.index.keyword.queryNodes('docs', 'words', 10) YIELD node RETURN node";
    assert_eq!(
        replica
            .execute(search, &Parameters::new())
            .unwrap_err()
            .kind(),
        ErrorKind::IndexNotFound
    );
    writer
        .create_keyword_index("docs", IndexTarget::Label("Doc"), &["text"])
        .unwrap();
    sync(&writer, &replica);
    assert_eq!(
        replica
            .execute(search, &Parameters::new())
            .unwrap()
            .rows()
            .len(),
        1
    );
    writer.drop_index("docs").unwrap();
    sync(&writer, &replica);
    assert_eq!(
        replica
            .execute(search, &Parameters::new())
            .unwrap_err()
            .kind(),
        ErrorKind::IndexNotFound
    );
}

#[test]
fn a_replica_on_disk_keeps_its_position_and_history_across_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let writer = Database::in_memory().unwrap();
    writer.promote(3, b"w").unwrap();
    writer.execute("INSERT (:A)", &Parameters::new()).unwrap();
    let path = dir.path().join("replica.rhizodb");
    let replica = replica_at(&path);
    sync(&writer, &replica);
    drop(replica);
    let replica = replica_at(&path);
    assert_eq!(replica.position(), writer.position());
    assert_eq!(
        replica.promote(2, b"x").unwrap_err().kind(),
        ErrorKind::Fenced
    );
    writer.execute("INSERT (:B)", &Parameters::new()).unwrap();
    sync(&writer, &replica);
    assert_eq!(dump(&replica), dump(&writer));
    drop(replica);
    let promoted = Database::open(&path).unwrap();
    promoted.promote(4, b"next").unwrap();
    promoted.execute("INSERT (:C)", &Parameters::new()).unwrap();
    assert_eq!(promoted.position().term(), 4);
    drop(promoted);
    let reopened = Database::open(&path).unwrap();
    assert_eq!(reopened.position().term(), 4);
    assert_eq!(reopened.position().writer_id(), b"next");
}
