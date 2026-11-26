use rhizodb::Database;
use rhizodb::ErrorKind;
use rhizodb::IndexTarget;
use rhizodb::OpenOptions;
use rhizodb::Parameters;

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
