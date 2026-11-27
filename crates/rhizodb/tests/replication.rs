use rhizodb::Database;
use rhizodb::ErrorKind;
use rhizodb::History;
use rhizodb::IndexTarget;
use rhizodb::Lineage;
use rhizodb::OpenOptions;
use rhizodb::Parameters;
use rhizodb::Position;
use rhizodb::ReplicationFailure;
use rhizodb::Role;

const WAIT: std::time::Duration = std::time::Duration::from_secs(5);

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
    let error = OpenOptions::new().replica(true).open(&path).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NotReplica);
    let writer = Database::open(&path).unwrap();
    assert_eq!(writer.role(), Role::Writer);
    writer.demote(WAIT).unwrap();
    drop(writer);
    let replica = Database::open(&path).unwrap();
    assert_eq!(replica.role(), Role::Replica);
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
        assert_eq!(error.kind(), ErrorKind::NotWriter, "{error}");
    }
    replica.begin_read().unwrap();
    replica.check().unwrap();
}

#[test]
fn a_file_remembers_its_role_across_reopens() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("graph.rhizodb");
    let replica = replica_at(&path);
    replica.check().unwrap();
    assert_eq!(replica.position(), Position::start());
    drop(replica);
    let reopened = Database::open(&path).unwrap();
    assert_eq!(reopened.role(), Role::Replica);
    assert_eq!(
        reopened
            .execute("INSERT (:A)", &Parameters::new())
            .unwrap_err()
            .kind(),
        ErrorKind::NotWriter
    );
    reopened.promote(1, b"me").unwrap();
    reopened.execute("INSERT (:A)", &Parameters::new()).unwrap();
    drop(reopened);
    let writer = Database::open(&path).unwrap();
    assert_eq!(writer.role(), Role::Fenced);
    assert_eq!(
        writer
            .execute("INSERT (:B)", &Parameters::new())
            .unwrap_err()
            .kind(),
        ErrorKind::NotWriter
    );
    writer.promote(1, b"me").unwrap();
    assert_eq!(writer.role(), Role::Writer);
    writer.execute("INSERT (:B)", &Parameters::new()).unwrap();
    let rows = writer
        .execute("MATCH (n) RETURN count(n)", &Parameters::new())
        .unwrap();
    assert_eq!(rows.rows(), [vec![rhizodb::Value::Integer(2)]]);
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
    let last = a.demote(WAIT).unwrap();
    assert_eq!(
        a.execute("INSERT ()", &Parameters::new())
            .unwrap_err()
            .kind(),
        ErrorKind::NotWriter
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
    b.demote(WAIT).unwrap();
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

#[test]
fn refusals_carry_the_positions_and_lsns_behind_them() {
    let a = Database::in_memory().unwrap();
    a.promote(1, b"a").unwrap();
    a.execute("INSERT (:One)", &Parameters::new()).unwrap();
    let held = a.position();
    a.execute("INSERT (:Two)", &Parameters::new()).unwrap();
    let fresh = in_memory_replica();
    let error = fresh
        .apply(&a.changes_since(&held, 1 << 20).unwrap())
        .unwrap_err();
    assert!(matches!(
        error.replication_failure(),
        Some(ReplicationFailure::Gap {
            expected_lsn: 1,
            ..
        })
    ));
    let b = in_memory_replica();
    sync(&a, &b);
    b.promote(2, b"b").unwrap();
    let error = b.promote(1, b"x").unwrap_err();
    assert!(matches!(
        error.replication_failure(),
        Some(ReplicationFailure::Fenced { seen_term: 2, .. })
    ));
    b.execute("INSERT (:Three)", &Parameters::new()).unwrap();
    a.execute("INSERT (:Stale)", &Parameters::new()).unwrap();
    let error = b.changes_since(&a.position(), 1 << 20).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Diverged);
    let Some(ReplicationFailure::Diverged {
        local,
        remote,
        fork_lsn,
        ..
    }) = error.replication_failure()
    else {
        panic!("{error}");
    };
    assert_eq!((local, remote), (b.position(), a.position()));
    assert_eq!(fork_lsn, b.history().entries()[1].start_lsn());
    assert!(
        error
            .to_string()
            .ends_with(&format!("from LSN {fork_lsn} onward")),
        "{error}"
    );
    let _ = a.changes_since(&b.position(), 1 << 20);
    assert_eq!(
        a.execute("INSERT ()", &Parameters::new())
            .unwrap_err()
            .kind(),
        ErrorKind::NotWriter
    );
    assert_eq!(a.role(), Role::Fenced);
}

#[test]
fn lineages_compare_by_history_and_position() {
    let a = Database::in_memory().unwrap();
    a.promote(1, b"a").unwrap();
    a.execute("INSERT (:One)", &Parameters::new()).unwrap();
    let b = in_memory_replica();
    sync(&a, &b);
    let history = |db: &Database| History::from_bytes(&db.history().to_bytes()).unwrap();
    assert_eq!(
        b.compare_lineage(&history(&a), &a.position()),
        Lineage::Same
    );
    a.execute("INSERT (:Two)", &Parameters::new()).unwrap();
    assert_eq!(
        b.compare_lineage(&history(&a), &a.position()),
        Lineage::Behind
    );
    assert_eq!(
        a.compare_lineage(&history(&b), &b.position()),
        Lineage::Ahead
    );
    b.promote(2, b"b").unwrap();
    let fork = b.position().lsn();
    assert!(matches!(
        b.compare_lineage(&history(&a), &a.position()),
        Lineage::Diverged { fork_lsn, .. } if fork_lsn == fork
    ));
}

#[test]
fn a_transaction_larger_than_the_limit_travels_whole() {
    let writer = Database::in_memory().unwrap();
    writer.promote(1, b"w").unwrap();
    let big = Parameters::from([(
        "text".to_owned(),
        rhizodb::Value::String("x".repeat(3 << 20)),
    )]);
    writer
        .execute(
            "UNWIND range(1, 4) AS i INSERT (:Big {i: i, text: $text})",
            &big,
        )
        .unwrap();
    let replica = in_memory_replica();
    let mut calls = 0;
    loop {
        let changes = writer.changes_since(&replica.position(), 1024).unwrap();
        if changes.is_empty() {
            break;
        }
        assert!(changes.len() == 1 || changes.as_bytes().len() < 4096);
        replica.apply(&changes).unwrap();
        calls += 1;
    }
    assert!(calls >= 3);
    assert_eq!(dump(&replica), dump(&writer));
}

#[test]
fn demotion_refuses_the_open_transaction_and_waits_for_it_to_end() {
    let writer = std::sync::Arc::new(Database::in_memory().unwrap());
    writer.promote(1, b"w").unwrap();
    let (began, wait_for_began) = std::sync::mpsc::channel();
    let (finish, wait_for_finish) = std::sync::mpsc::channel::<()>();
    let held = std::sync::Arc::clone(&writer);
    let thread = std::thread::spawn(move || {
        let mut tx = held.begin_write().unwrap();
        tx.execute("INSERT (:InFlight)", &Parameters::new())
            .unwrap();
        began.send(()).unwrap();
        wait_for_finish.recv().unwrap();
        tx.commit().unwrap_err().kind()
    });
    wait_for_began.recv().unwrap();
    let error = writer
        .demote(std::time::Duration::from_millis(20))
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::WriterBusy);
    assert_eq!(writer.role(), Role::Fenced);
    finish.send(()).unwrap();
    assert_eq!(thread.join().unwrap(), ErrorKind::NotWriter);
    let last = writer.demote(WAIT).unwrap();
    assert_eq!(writer.role(), Role::Replica);
    let rows = writer
        .execute("MATCH (n:InFlight) RETURN count(n)", &Parameters::new())
        .unwrap();
    assert_eq!(rows.rows(), [vec![rhizodb::Value::Integer(0)]]);
    let next = in_memory_replica();
    assert_eq!(sync(&writer, &next), last);
    next.promote(2, b"next").unwrap();
}

#[test]
fn reads_run_while_a_replica_applies_index_creations_and_drops() {
    let writer = Database::in_memory().unwrap();
    writer.promote(1, b"w").unwrap();
    writer
        .execute(
            "UNWIND range(1, 200) AS i INSERT (:Doc {i: i, text: 'word ' + toString(i)})",
            &Parameters::new(),
        )
        .unwrap();
    let replica = std::sync::Arc::new(in_memory_replica());
    sync(&writer, &replica);
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let readers: Vec<_> = (0..3)
        .map(|_| {
            let replica = std::sync::Arc::clone(&replica);
            let stop = std::sync::Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut reads = 0;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    for query in [
                        "CALL db.index.keyword.queryNodes('docs', 'word', 5) YIELD node RETURN count(node)",
                        "MATCH (d:Doc) WHERE d.i = 7 RETURN d.i",
                    ] {
                        match replica.execute(query, &Parameters::new()) {
                            Ok(_) => reads += 1,
                            Err(error) => {
                                assert_eq!(error.kind(), ErrorKind::IndexNotFound, "{error}")
                            }
                        }
                    }
                }
                reads
            })
        })
        .collect();
    for _ in 0..20 {
        writer
            .create_keyword_index("docs", IndexTarget::Label("Doc"), &["text"])
            .unwrap();
        writer
            .create_property_index("by_i", IndexTarget::Label("Doc"), "i")
            .unwrap();
        sync(&writer, &replica);
        writer.drop_index("docs").unwrap();
        writer.drop_index("by_i").unwrap();
        sync(&writer, &replica);
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let reads: usize = readers
        .into_iter()
        .map(|reader| reader.join().unwrap())
        .sum();
    assert!(reads > 0);
    assert_eq!(dump(&replica), dump(&writer));
}

#[test]
fn a_replica_refuses_the_changes_of_an_unrelated_database() {
    let x = Database::in_memory().unwrap();
    x.promote(1, b"x").unwrap();
    x.execute("INSERT (:X)", &Parameters::new()).unwrap();
    let y = Database::in_memory().unwrap();
    y.promote(1, b"y").unwrap();
    y.execute("INSERT (:Y)", &Parameters::new()).unwrap();
    let replica = in_memory_replica();
    assert_eq!(replica.position().database_id(), 0);
    sync(&x, &replica);
    assert_eq!(replica.position().database_id(), x.position().database_id());
    assert_ne!(x.position().database_id(), y.position().database_id());
    let error = y.changes_since(&replica.position(), 1 << 20).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::DifferentDatabase);
    assert!(matches!(
        error.replication_failure(),
        Some(ReplicationFailure::DifferentDatabase { local, remote, .. })
            if local == y.position().database_id() && remote == x.position().database_id()
    ));
    let foreign = y.changes_since(&Position::start(), 1 << 20).unwrap();
    assert_eq!(
        replica.apply(&foreign).unwrap_err().kind(),
        ErrorKind::DifferentDatabase
    );
    assert_eq!(
        replica.compare_lineage(&y.history(), &y.position()),
        Lineage::DifferentDatabase
    );
    assert_eq!(dump(&replica), dump(&x));
}

#[test]
fn a_file_of_an_older_format_opens_as_a_writer_whose_first_snapshot_a_replica_installs() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("old.rhizodb");
    std::fs::copy(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/engine-1-1.rhizodb"),
        &path,
    )
    .unwrap();
    let writer = Database::open(&path).unwrap();
    assert_eq!(writer.role(), Role::Writer);
    assert!(writer.history().entries().is_empty());
    assert_eq!(writer.position().term(), 0);
    writer.promote(1, b"w").unwrap();
    let snapshot = writer.snapshot().unwrap();
    let mut bytes = vec![0; usize::try_from(snapshot.len()).unwrap()];
    let mut offset = 0;
    while offset < bytes.len() {
        offset += snapshot
            .read_at(offset as u64, &mut bytes[offset..])
            .unwrap();
    }
    drop(snapshot);
    let replica =
        Database::install_snapshot(dir.path().join("replica.rhizodb"), bytes.as_slice()).unwrap();
    writer
        .execute("INSERT (:Added {v: 1})", &Parameters::new())
        .unwrap();
    assert_eq!(sync(&writer, &replica), writer.position());
    assert_eq!(dump(&replica), dump(&writer));
    drop(writer);
    let reopened = Database::open(&path).unwrap();
    assert_eq!(reopened.role(), Role::Fenced);
    assert_eq!(reopened.history().entries().len(), 1);
}

#[test]
fn two_files_that_began_one_term_under_one_writer_id_are_told_apart_by_their_incarnations() {
    let writer = Database::in_memory().unwrap();
    writer.promote(1, b"w").unwrap();
    writer
        .execute("INSERT (:N {k: 1})", &Parameters::new())
        .unwrap();
    let a = in_memory_replica();
    let b = in_memory_replica();
    sync(&writer, &a);
    sync(&writer, &b);
    a.promote(5, b"x").unwrap();
    b.promote(5, b"x").unwrap();
    for k in 2..5 {
        b.execute(
            "INSERT (:N {k: $k})",
            &Parameters::from([("k".to_owned(), rhizodb::Value::Integer(k))]),
        )
        .unwrap();
    }
    let history = History::from_bytes(&b.history().to_bytes()).unwrap();
    assert_eq!(history, b.history());
    assert!(matches!(
        a.compare_lineage(&history, &b.position()),
        Lineage::Diverged { .. }
    ));
}

#[test]
fn a_demotion_that_may_wait_longer_than_the_clock_reaches_finishes() {
    let writer = Database::in_memory().unwrap();
    writer.promote(1, b"w").unwrap();
    writer.demote(std::time::Duration::MAX).unwrap();
    assert_eq!(writer.role(), Role::Replica);
}

fn insert(db: &Database, k: i64) {
    db.execute(
        "INSERT (:N {k: $k})",
        &Parameters::from([("k".to_owned(), rhizodb::Value::Integer(k))]),
    )
    .unwrap();
}

#[test]
fn a_caught_up_writer_fenced_by_a_failover_rejoins_as_a_replica_without_a_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("old.rhizodb");
    let old = Database::open(&path).unwrap();
    old.promote(1, b"a").unwrap();
    for k in 0..10 {
        insert(&old, k);
    }
    let next = in_memory_replica();
    sync(&old, &next);
    next.promote(2, b"b").unwrap();
    insert(&next, 10);
    old.acknowledge(b"b", &next.position()).unwrap();
    assert_eq!(old.role(), Role::Fenced);
    old.demote(WAIT).unwrap();
    assert_eq!(old.role(), Role::Replica);
    assert_eq!(
        old.compare_lineage(&next.history(), &next.position()),
        Lineage::Behind
    );
    assert_eq!(sync(&next, &old), next.position());
    assert_eq!(dump(&old), dump(&next));
    drop(old);
    let old = Database::open(&path).unwrap();
    assert_eq!(old.role(), Role::Replica);
    insert(&next, 11);
    assert_eq!(sync(&next, &old), next.position());
    assert_eq!(dump(&old), dump(&next));
    let error = old.promote(1, b"a").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Fenced);
    old.promote(3, b"a").unwrap();
    insert(&old, 12);
    drop(old);
    assert_eq!(Database::open(&path).unwrap().role(), Role::Fenced);
}

#[test]
fn a_writer_fenced_by_a_failover_with_changes_the_new_writer_lacks_still_diverges() {
    let old = Database::in_memory().unwrap();
    old.promote(1, b"a").unwrap();
    insert(&old, 0);
    let next = in_memory_replica();
    sync(&old, &next);
    insert(&old, 1);
    next.promote(2, b"b").unwrap();
    insert(&next, 2);
    old.acknowledge(b"b", &next.position()).unwrap();
    old.demote(WAIT).unwrap();
    assert_eq!(old.role(), Role::Replica);
    assert!(matches!(
        old.compare_lineage(&next.history(), &next.position()),
        Lineage::Diverged { .. }
    ));
    let error = next.changes_since(&old.position(), 1 << 20).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Diverged);
    let changes = next.changes_since(&Position::start(), 1 << 20).unwrap();
    let error = old.apply(&changes).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Diverged);
}

#[test]
fn a_replica_that_heard_of_a_term_applies_the_first_change_of_that_term() {
    let writer = Database::in_memory().unwrap();
    writer.promote(1, b"a").unwrap();
    insert(&writer, 0);
    let next = in_memory_replica();
    let replica = in_memory_replica();
    sync(&writer, &next);
    sync(&writer, &replica);
    next.promote(2, b"b").unwrap();
    replica.acknowledge(b"b", &next.position()).unwrap();
    assert_eq!(sync(&next, &replica), next.position());
}
