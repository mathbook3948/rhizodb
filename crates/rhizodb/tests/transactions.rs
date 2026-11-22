mod support;

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use rhizodb::Database;
use rhizodb::ErrorKind;
use rhizodb::Parameters;
use support::*;

const COUNT: &str = "MATCH (n) RETURN count(n)";

fn count(db: &Database) -> i64 {
    ints(&run(db, COUNT))[0]
}

#[test]
fn a_write_transaction_sees_its_own_writes_and_others_see_them_after_commit() {
    let db = db();
    let mut tx = db.begin_write().unwrap();
    tx.execute("CREATE (:A)", &Parameters::new()).unwrap();
    tx.execute("CREATE (:A)", &Parameters::new()).unwrap();
    let inside = tx.execute(COUNT, &Parameters::new()).unwrap();
    assert_eq!(inside.rows(), [vec![int(2)]]);
    assert_eq!(count(&db), 0);
    tx.commit().unwrap();
    assert_eq!(count(&db), 2);
}

#[test]
fn a_rolled_back_write_transaction_leaves_no_trace() {
    let db = db();
    let mut tx = db.begin_write().unwrap();
    tx.execute("CREATE (:A)", &Parameters::new()).unwrap();
    tx.rollback().unwrap();
    assert_eq!(count(&db), 0);
}

#[test]
fn a_write_transaction_dropped_without_commit_rolls_back() {
    let db = db();
    {
        let mut tx = db.begin_write().unwrap();
        tx.execute("CREATE (:A)", &Parameters::new()).unwrap();
    }
    assert_eq!(count(&db), 0);
    run(&db, "CREATE (:B)");
    assert_eq!(count(&db), 1);
}

#[test]
fn a_read_transaction_keeps_the_snapshot_it_began_with() {
    let db = db();
    run(&db, "CREATE (:A)");
    let reader = db.begin_read().unwrap();
    run(&db, "CREATE (:A)");
    let mut writer = db.begin_write().unwrap();
    writer.execute("CREATE (:A)", &Parameters::new()).unwrap();
    writer.commit().unwrap();
    for _ in 0..2 {
        let seen = reader.execute(COUNT, &Parameters::new()).unwrap();
        assert_eq!(seen.rows(), [vec![int(1)]]);
    }
    assert_eq!(count(&db), 3);
    let later = db.begin_read().unwrap();
    let seen = later.execute(COUNT, &Parameters::new()).unwrap();
    assert_eq!(seen.rows(), [vec![int(3)]]);
}

#[test]
fn an_updating_query_in_a_read_transaction_fails_and_leaves_it_usable() {
    let db = db();
    let reader = db.begin_read().unwrap();
    let error = reader
        .execute("CREATE (:A)", &Parameters::new())
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::ReadOnlyTransaction);
    let error = reader
        .execute("RETURN 1 / 0", &Parameters::new())
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::DivisionByZero);
    let seen = reader.execute(COUNT, &Parameters::new()).unwrap();
    assert_eq!(seen.rows(), [vec![int(0)]]);
    assert_eq!(count(&db), 0);
}

#[test]
fn a_failed_query_rolls_back_the_whole_write_transaction() {
    for failing in [
        "CREATE (:B {x: 1 / 0})",
        "RETURN 1 / 0",
        "RETURN undefined",
        "RETURN",
    ] {
        let db = db();
        let mut tx = db.begin_write().unwrap();
        tx.execute("CREATE (:A)", &Parameters::new()).unwrap();
        assert!(
            tx.execute(failing, &Parameters::new()).is_err(),
            "{failing}"
        );
        let error = tx.execute(COUNT, &Parameters::new()).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::TransactionAborted, "{failing}");
        assert_eq!(
            tx.commit().unwrap_err().kind(),
            ErrorKind::TransactionAborted,
            "{failing}"
        );
        assert_eq!(count(&db), 0, "{failing}");
    }
}

#[test]
fn rolling_back_a_transaction_a_failed_query_ended_succeeds() {
    let db = db();
    let mut tx = db.begin_write().unwrap();
    assert!(tx.execute("RETURN 1 / 0", &Parameters::new()).is_err());
    tx.rollback().unwrap();
}

#[test]
fn a_failed_query_frees_the_writer_for_others() {
    let db = db();
    let mut tx = db.begin_write().unwrap();
    assert!(tx.execute("RETURN 1 / 0", &Parameters::new()).is_err());
    run(&db, "CREATE (:A)");
    assert_eq!(count(&db), 1);
}

#[test]
fn a_second_write_transaction_waits_until_the_first_ends() {
    let db = db();
    let first = db.begin_write().unwrap();
    let (began, waited) = mpsc::channel();
    thread::scope(|scope| {
        scope.spawn(|| {
            let mut second = db.begin_write().unwrap();
            began.send(()).unwrap();
            second
                .execute("CREATE (:Second)", &Parameters::new())
                .unwrap();
            second.commit().unwrap();
        });
        assert!(waited.recv_timeout(Duration::from_millis(200)).is_err());
        first.commit().unwrap();
        waited.recv_timeout(Duration::from_secs(10)).unwrap();
    });
    assert_eq!(count(&db), 1);
}

#[test]
fn a_committed_write_transaction_survives_reopening() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("graph.redb");
    {
        let db = Database::open(&path).unwrap();
        let mut tx = db.begin_write().unwrap();
        tx.execute("CREATE (:A)", &Parameters::new()).unwrap();
        tx.execute("MATCH (a:A) CREATE (a)-[:T]->(:B)", &Parameters::new())
            .unwrap();
        tx.commit().unwrap();
    }
    let db = Database::open(&path).unwrap();
    assert_eq!(count(&db), 2);
}

#[test]
fn a_write_on_the_thread_that_holds_the_write_transaction_fails_instead_of_waiting() {
    let db = db();
    let mut tx = db.begin_write().unwrap();
    tx.execute("CREATE (:A)", &Parameters::new()).unwrap();
    assert_eq!(
        db.execute("CREATE (:B)", &Parameters::new())
            .unwrap_err()
            .kind(),
        ErrorKind::WriteTransactionOnThisThread
    );
    assert_eq!(
        db.begin_write().unwrap_err().kind(),
        ErrorKind::WriteTransactionOnThisThread
    );
    assert_eq!(count(&db), 0);
    tx.commit().unwrap();
    run(&db, "CREATE (:B)");
    assert_eq!(count(&db), 2);
}

#[test]
fn the_transaction_clock_reads_one_instant_for_every_query_of_a_transaction() {
    let db = db();
    let mut tx = db.begin_write().unwrap();
    tx.execute(
        "CREATE (:T {at: datetime.transaction()})",
        &Parameters::new(),
    )
    .unwrap();
    thread::sleep(Duration::from_millis(5));
    let same = tx
        .execute(
            "MATCH (t:T) RETURN t.at = datetime.transaction(), t.at < datetime.statement()",
            &Parameters::new(),
        )
        .unwrap();
    assert_eq!(
        same.rows(),
        [vec![
            rhizodb::Value::Boolean(true),
            rhizodb::Value::Boolean(true)
        ]]
    );
    tx.commit().unwrap();
    let read = db.begin_read().unwrap();
    let first = read
        .execute("RETURN datetime.transaction() AS t", &Parameters::new())
        .unwrap();
    thread::sleep(Duration::from_millis(5));
    let second = read
        .execute("RETURN datetime.transaction() AS t", &Parameters::new())
        .unwrap();
    assert_eq!(first.rows(), second.rows());
}

#[test]
fn a_clock_call_without_arguments_reads_the_statement_clock_inside_a_transaction() {
    let db = db();
    let read = db.begin_read().unwrap();
    thread::sleep(Duration::from_millis(5));
    let clocks = read
        .execute(
            "RETURN datetime() = datetime.statement(), datetime() > datetime.transaction()",
            &Parameters::new(),
        )
        .unwrap();
    assert_eq!(
        clocks.rows(),
        [vec![
            rhizodb::Value::Boolean(true),
            rhizodb::Value::Boolean(true)
        ]]
    );
}

#[test]
fn a_read_transaction_does_not_see_names_committed_after_it_began() {
    let db = db();
    let before = db.begin_read().unwrap();
    run(&db, "INSERT (:Fresh {fresh: 1})-[:FRESH]->()");
    assert_eq!(
        run(&db, "MATCH (n:Fresh)-[:FRESH]->() RETURN n.fresh"),
        [vec![int(1)]]
    );
    let result = before
        .execute("MATCH (n:Fresh) RETURN n.fresh", &Parameters::new())
        .unwrap();
    assert!(result.rows().is_empty());
    let result = before
        .execute("MATCH ()-[r:FRESH]->() RETURN r", &Parameters::new())
        .unwrap();
    assert!(result.rows().is_empty());
}

#[test]
fn a_name_from_a_rolled_back_write_does_not_find_a_later_name() {
    let db = db();
    let mut write = db.begin_write().unwrap();
    write
        .execute("INSERT (:Gone {gone: 1})", &Parameters::new())
        .unwrap();
    assert_eq!(
        ints(
            write
                .execute("MATCH (n:Gone) RETURN n.gone", &Parameters::new())
                .unwrap()
                .rows()
        ),
        [1]
    );
    write.rollback().unwrap();
    run(&db, "INSERT (:Kept {kept: 2})");
    assert!(run(&db, "MATCH (n:Gone) RETURN n").is_empty());
    assert!(
        run(&db, "MATCH (n) RETURN n.gone")
            .iter()
            .all(|row| row[0] == rhizodb::Value::Null)
    );
    assert_eq!(ints(&run(&db, "MATCH (n:Kept) RETURN n.kept")), [2]);
}
