use std::io;
use std::io::Read;

use rhizodb::Database;
use rhizodb::ErrorKind;
use rhizodb::OpenOptions;
use rhizodb::Parameters;
use rhizodb::Position;
use rhizodb::ReplicationFailure;
use rhizodb::Snapshot;
use rhizodb::Value;

fn stream(snapshot: &Snapshot<'_>) -> Vec<u8> {
    let mut bytes = vec![0; usize::try_from(snapshot.len()).unwrap()];
    let mut offset = 0;
    while offset < bytes.len() {
        let end = bytes.len().min(offset + 9_000);
        let read = snapshot
            .read_at(offset as u64, &mut bytes[offset..end])
            .unwrap();
        assert!(read > 0);
        offset += read;
    }
    assert_eq!(snapshot.read_at(snapshot.len(), &mut [0; 4]).unwrap(), 0);
    bytes
}

fn count(db: &Database, query: &str) -> Value {
    db.execute(query, &Parameters::new()).unwrap().rows()[0][0].clone()
}

fn load(db: &Database, from: i64, to: i64) {
    let parameters = Parameters::from([
        ("from".to_owned(), Value::Integer(from)),
        ("to".to_owned(), Value::Integer(to)),
    ]);
    db.execute(
        "UNWIND range($from, $to) AS i INSERT (:Item {i: i, text: 'item ' + toString(i)})",
        &parameters,
    )
    .unwrap();
}

#[test]
fn a_replica_installed_from_a_snapshot_answers_as_the_source_did_at_its_checkpoint() {
    let directory = tempfile::tempdir().unwrap();
    let source = Database::open(directory.path().join("source.rhizodb")).unwrap();
    load(&source, 1, 3000);
    source.compact().unwrap();
    let snapshot = source.snapshot().unwrap();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            for round in 0..20 {
                load(&source, 10_000 + round * 100, 10_000 + round * 100 + 99);
                source
                    .execute(
                        "MATCH (n:Item) WHERE n.i % 7 = $r DELETE n",
                        &Parameters::from([("r".to_owned(), Value::Integer(round % 7))]),
                    )
                    .unwrap();
                if round % 5 == 4 {
                    source.compact().unwrap();
                }
            }
        });
    });
    let bytes = stream(&snapshot);
    drop(snapshot);
    let replica_path = directory.path().join("replica.rhizodb");
    let replica = Database::install_snapshot(&replica_path, bytes.as_slice()).unwrap();
    replica.check().unwrap();
    assert_eq!(
        count(&replica, "MATCH (n:Item) RETURN count(n)"),
        Value::Integer(3000)
    );
    assert_eq!(
        count(&replica, "MATCH (n:Item) RETURN sum(n.i)"),
        Value::Integer(4_501_500)
    );
    let error = replica
        .execute("INSERT (:Item)", &Parameters::new())
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NotWriter);
    drop(replica);
    let reopened = Database::open(&replica_path).unwrap();
    assert_eq!(
        count(&reopened, "MATCH (n:Item) RETURN count(n)"),
        Value::Integer(3000)
    );
    let replica_snapshot = reopened.snapshot().unwrap();
    let relayed = Database::install_snapshot(
        directory.path().join("relayed.rhizodb"),
        stream(&replica_snapshot).as_slice(),
    )
    .unwrap();
    assert_eq!(
        count(&relayed, "MATCH (n:Item) RETURN count(n)"),
        Value::Integer(3000)
    );
}

#[test]
fn installing_fails_with_the_kind_of_its_cause() {
    let directory = tempfile::tempdir().unwrap();
    let source = Database::open(directory.path().join("source.rhizodb")).unwrap();
    load(&source, 1, 100);
    source.compact().unwrap();
    let bytes = stream(&source.snapshot().unwrap());
    let target = directory.path().join("target.rhizodb");
    let mut damaged = bytes.clone();
    damaged[bytes.len() / 2] ^= 4;
    let error = Database::install_snapshot(&target, damaged.as_slice()).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::CorruptData);
    let error = Database::install_snapshot(&target, &bytes[..bytes.len() - 1]).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::CorruptData);
    struct Failing;
    impl Read for Failing {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::ConnectionReset, "cut"))
        }
    }
    assert_eq!(
        Database::install_snapshot(&target, Failing)
            .unwrap_err()
            .kind(),
        ErrorKind::Io
    );
    let open = Database::install_snapshot(&target, bytes.as_slice()).unwrap();
    let again = Database::install_snapshot(&target, bytes.as_slice()).unwrap_err();
    assert_eq!(again.kind(), ErrorKind::DatabaseAlreadyOpen);
    assert_eq!(
        count(&open, "MATCH (n:Item) RETURN count(n)"),
        Value::Integer(100)
    );
    drop(open);
    OpenOptions::new()
        .max_retained_bytes(0)
        .install_snapshot(&target, bytes.as_slice())
        .unwrap();
}

#[test]
fn a_peer_id_holds_one_to_sixty_four_bytes() {
    let db = Database::in_memory().unwrap();
    db.acknowledge(b"replica-a", &Position::start()).unwrap();
    db.acknowledge(&[7; 64], &Position::start()).unwrap();
    assert!(db.forget_peer(b"replica-a").unwrap());
    assert!(!db.forget_peer(b"replica-a").unwrap());
    for bad in [&[][..], &[1; 65][..]] {
        assert_eq!(
            db.acknowledge(bad, &Position::start()).unwrap_err().kind(),
            ErrorKind::InvalidPeerId
        );
        assert_eq!(
            db.forget_peer(bad).unwrap_err().kind(),
            ErrorKind::InvalidPeerId
        );
    }
    for peer in 0..31u8 {
        db.acknowledge(&[peer], &Position::start()).unwrap();
    }
    let error = db.acknowledge(b"one more", &Position::start()).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::TooManyPeers);
    db.acknowledge(&[5], &Position::start()).unwrap();
    assert!(db.forget_peer(&[5]).unwrap());
    db.acknowledge(b"one more", &Position::start()).unwrap();
}

fn follow(writer: &Database, replica: &Database) {
    loop {
        let changes = writer.changes_since(&replica.position(), 1 << 20).unwrap();
        if changes.is_empty() {
            return;
        }
        replica.apply(&changes).unwrap();
    }
}

#[test]
fn a_replica_seeded_by_a_snapshot_follows_its_writer_and_seeds_the_next_replica() {
    let directory = tempfile::tempdir().unwrap();
    let writer = Database::open(directory.path().join("writer.rhizodb")).unwrap();
    writer.promote(1, b"node-a").unwrap();
    load(&writer, 1, 500);
    writer.compact().unwrap();
    let snapshot = writer.snapshot().unwrap();
    assert_eq!(
        (snapshot.position().term(), snapshot.position().writer_id()),
        (1, &b"node-a"[..])
    );
    let bytes = stream(&snapshot);
    let position = snapshot.position();
    writer.acknowledge(b"replica", &position).unwrap();
    drop(snapshot);
    let replica =
        Database::install_snapshot(directory.path().join("replica.rhizodb"), bytes.as_slice())
            .unwrap();
    assert_eq!(replica.position(), position);
    load(&writer, 501, 800);
    follow(&writer, &replica);
    assert_eq!(replica.position(), writer.position());
    assert_eq!(
        count(&replica, "MATCH (n:Item) RETURN count(n)"),
        Value::Integer(800)
    );
    replica.compact().unwrap();
    let relayed_bytes = stream(&replica.snapshot().unwrap());
    let relayed = Database::install_snapshot(
        directory.path().join("relayed.rhizodb"),
        relayed_bytes.as_slice(),
    )
    .unwrap();
    load(&writer, 801, 900);
    follow(&writer, &replica);
    follow(&replica, &relayed);
    assert_eq!(relayed.position(), writer.position());
    assert_eq!(
        count(&relayed, "MATCH (n:Item) RETURN count(n)"),
        Value::Integer(900)
    );
}

#[test]
fn a_restarted_writer_still_serves_a_peer_that_lags_behind() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("writer.rhizodb");
    let writer = Database::open(&path).unwrap();
    writer.promote(1, b"node-a").unwrap();
    load(&writer, 1, 10);
    let lagging = writer.position();
    writer.acknowledge(b"replica", &lagging).unwrap();
    for round in 0..40 {
        load(&writer, 100 + round * 50, 149 + round * 50);
        writer.compact().unwrap();
    }
    drop(writer);
    let writer = Database::open(&path).unwrap();
    writer.promote(1, b"node-a").unwrap();
    let changes = writer.changes_since(&lagging, 1 << 30).unwrap();
    assert_eq!(changes.after(), &lagging);
    let unpinned = tempfile::tempdir().unwrap();
    let other = Database::open(unpinned.path().join("other.rhizodb")).unwrap();
    other.promote(1, b"node-a").unwrap();
    load(&other, 1, 10);
    let behind = other.position();
    for round in 0..40 {
        load(&other, 100 + round * 50, 149 + round * 50);
        other.compact().unwrap();
    }
    drop(other);
    let other = Database::open(unpinned.path().join("other.rhizodb")).unwrap();
    let error = other.changes_since(&behind, 1 << 30).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::TooFarBehind);
    assert!(
        matches!(
            error.replication_failure(),
            Some(ReplicationFailure::TooFarBehind { oldest_lsn, .. }) if oldest_lsn > behind.lsn() + 1
        ),
        "{error}"
    );
}

#[test]
fn an_install_carries_the_newest_term_forward_and_refuses_an_older_snapshot_unless_forced() {
    let directory = tempfile::tempdir().unwrap();
    let old_writer = Database::open(directory.path().join("old.rhizodb")).unwrap();
    old_writer.promote(1, b"node-a").unwrap();
    load(&old_writer, 1, 20);
    old_writer.compact().unwrap();
    let old_bytes = stream(&old_writer.snapshot().unwrap());
    let new_writer = Database::open(directory.path().join("new.rhizodb")).unwrap();
    new_writer.promote(3, b"node-b").unwrap();
    load(&new_writer, 1, 30);
    new_writer.compact().unwrap();
    let new_bytes = stream(&new_writer.snapshot().unwrap());
    let path = directory.path().join("replica.rhizodb");
    let replica = Database::install_snapshot(&path, new_bytes.as_slice()).unwrap();
    assert_eq!(replica.role(), rhizodb::Role::Replica);
    assert_eq!(replica.position().term(), 3);
    drop(replica);
    let error = Database::install_snapshot(&path, old_bytes.as_slice()).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Fenced);
    let kept = OpenOptions::new().replica(true).open(&path).unwrap();
    assert_eq!(
        count(&kept, "MATCH (n:Item) RETURN count(n)"),
        Value::Integer(30)
    );
    drop(kept);
    let forced = OpenOptions::new()
        .force_install(true)
        .install_snapshot(&path, old_bytes.as_slice())
        .unwrap();
    assert_eq!(
        count(&forced, "MATCH (n:Item) RETURN count(n)"),
        Value::Integer(20)
    );
    load(&old_writer, 21, 25);
    let stale = old_writer
        .changes_since(&forced.position(), 1 << 20)
        .unwrap();
    assert!(!stale.is_empty());
    let error = forced.apply(&stale).unwrap_err();
    assert_eq!(
        error.kind(),
        ErrorKind::Fenced,
        "a stale writer's changes were applied"
    );
}

#[test]
fn acknowledging_a_newer_term_fences_a_writer() {
    let db = Database::in_memory().unwrap();
    db.promote(2, b"node-a").unwrap();
    load(&db, 1, 3);
    let newer = rhizodb::Position::from_bytes(&db.position().to_bytes()).unwrap();
    db.acknowledge(b"peer", &newer).unwrap();
    assert_eq!(db.role(), rhizodb::Role::Writer);
    let other = Database::in_memory().unwrap();
    other.promote(5, b"node-b").unwrap();
    db.acknowledge(b"peer", &other.position()).unwrap();
    assert_eq!(db.role(), rhizodb::Role::Fenced);
}

#[test]
fn an_install_refuses_a_snapshot_older_than_the_terms_the_replaced_database_began() {
    let directory = tempfile::tempdir().unwrap();
    let writer = Database::open(directory.path().join("writer.rhizodb")).unwrap();
    writer.promote(1, b"node-a").unwrap();
    load(&writer, 1, 10);
    writer.compact().unwrap();
    let old_bytes = stream(&writer.snapshot().unwrap());
    let path = directory.path().join("replica.rhizodb");
    let replica = Database::install_snapshot(&path, old_bytes.as_slice()).unwrap();
    replica.promote(4, b"node-b").unwrap();
    replica.compact().unwrap();
    drop(replica);
    let error = Database::install_snapshot(&path, old_bytes.as_slice()).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Fenced);
}
