mod support;

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::thread;

use rhizodb::Database;
use rhizodb::ErrorKind;
use rhizodb::Value;
use support::*;

#[test]
fn database_is_shareable_across_threads() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Database>();
}

#[test]
fn committed_data_survives_reopening_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("graph.rhizodb");
    {
        let db = Database::open(&path).unwrap();
        run(
            &db,
            "CREATE (:Person {name: 'Ada'})-[:KNOWS {since: 1843}]->(:Person {name: 'Charles'})",
        );
    }
    let db = Database::open(&path).unwrap();
    assert_eq!(
        run(
            &db,
            "MATCH (a)-[r:KNOWS]->(b) RETURN a.name, r.since, b.name"
        ),
        [vec![string("Ada"), int(1843), string("Charles")]]
    );
    run(&db, "CREATE (:Person {name: 'Grace'})");
    let ids = run(&db, "MATCH (n) RETURN id(n) ORDER BY id(n)");
    assert_eq!(ints(&ids), [0, 1, 2]);
}

#[test]
fn a_failing_write_query_leaves_no_trace() {
    let db = db();
    run(&db, "CREATE (:Keep)");
    assert_eq!(
        error_kind(
            &db,
            "MATCH (k:Keep) SET k.x = 1 CREATE (:New) WITH k RETURN 1 / 0"
        ),
        ErrorKind::DivisionByZero
    );
    assert_eq!(
        run(&db, "MATCH (n) RETURN labels(n), keys(n)"),
        [vec![list([string("Keep")]), list([])]]
    );
    run(&db, "CREATE (:Second)");
    assert_eq!(
        ints(&run(&db, "MATCH (n) RETURN id(n) ORDER BY id(n)")),
        [0, 1],
        "the failed query's node ID was rolled back with it"
    );
}

#[test]
fn readers_see_a_write_completely_or_not_at_all() {
    const NODES: i64 = 5_000;
    let db = Arc::new(db());
    let done = Arc::new(AtomicBool::new(false));
    let reader = {
        let db = Arc::clone(&db);
        let done = Arc::clone(&done);
        thread::spawn(move || {
            let mut seen = Vec::new();
            while !done.load(Ordering::Acquire) {
                seen.push(single(&db, "MATCH (n) RETURN count(n)"));
            }
            seen.push(single(&db, "MATCH (n) RETURN count(n)"));
            seen
        })
    };
    run(
        &db,
        &format!("UNWIND range(1, {NODES}) AS i CREATE ({{i: i}})"),
    );
    done.store(true, Ordering::Release);
    let seen = reader.join().unwrap();
    assert!(
        seen.iter()
            .all(|count| *count == int(0) || *count == int(NODES)),
        "a reader saw a partial write: {:?}",
        seen.iter()
            .find(|count| **count != int(0) && **count != int(NODES))
    );
    assert_eq!(seen.last(), Some(&int(NODES)));
}

#[test]
fn results_carry_element_ids_labels_and_properties() {
    let db = db();
    let value = single(&db, "CREATE (n:A:B {x: 1}) RETURN n");
    let Value::Node(node) = value else {
        panic!("expected a node, got {value:?}");
    };
    assert_eq!(node.id().as_u64(), 0);
    assert_eq!(
        node.labels().iter().map(String::as_str).collect::<Vec<_>>(),
        ["A", "B"]
    );
    assert_eq!(node.properties()["x"], int(1));
    let value = single(&db, "MATCH (n:A) CREATE (n)-[r:R {w: 2}]->(m) RETURN r");
    let Value::Relationship(relationship) = value else {
        panic!("expected a relationship, got {value:?}");
    };
    assert_eq!(relationship.relationship_type(), "R");
    assert_eq!(relationship.start(), node.id());
    assert_ne!(relationship.end(), node.id());
    assert_eq!(relationship.properties()["w"], int(2));
}

#[test]
fn a_node_returned_earlier_can_be_passed_back_as_a_parameter() {
    let db = db();
    let node = single(&db, "CREATE (n {name: 'Ada'}) RETURN n");
    let mut parameters = rhizodb::Parameters::new();
    parameters.insert("n".to_owned(), node);
    let result = db
        .execute("MATCH (m) WHERE m = $n RETURN m.name", &parameters)
        .unwrap();
    assert_eq!(result.rows(), [vec![string("Ada")]]);
}

#[test]
fn a_deleted_node_passed_as_a_parameter_can_still_be_returned() {
    let db = db();
    let node = single(&db, "CREATE (n {name: 'Ada'}) RETURN n");
    let mut parameters = rhizodb::Parameters::new();
    parameters.insert("n".to_owned(), node.clone());
    let result = db
        .execute(
            "MATCH (m) WHERE m = $n DELETE m RETURN $n AS gone",
            &parameters,
        )
        .unwrap();
    assert_eq!(result.rows(), [vec![node]]);
    assert_eq!(single(&db, "MATCH (n) RETURN count(*)"), int(0));
}

#[test]
fn a_query_run_again_reads_the_parameters_of_each_run() {
    let db = db();
    let query = "UNWIND $xs AS x RETURN x + $y AS z, $y AS y";
    let run_with = |xs: Vec<i64>, y: i64| {
        let mut parameters = rhizodb::Parameters::new();
        parameters.insert("xs".to_owned(), list(xs.into_iter().map(int)));
        parameters.insert("y".to_owned(), int(y));
        db.execute(query, &parameters).unwrap().rows().to_vec()
    };
    assert_eq!(
        run_with(vec![1, 2], 10),
        [vec![int(11), int(10)], vec![int(12), int(10)]]
    );
    assert_eq!(run_with(vec![5], 1), [vec![int(6), int(1)]]);
    let transaction = db.begin_read().unwrap();
    let mut parameters = rhizodb::Parameters::new();
    parameters.insert("xs".to_owned(), list([int(0)]));
    parameters.insert("y".to_owned(), int(3));
    assert_eq!(
        transaction.execute(query, &parameters).unwrap().rows(),
        [vec![int(3), int(3)]]
    );
}

#[test]
fn a_query_run_again_without_a_parameter_it_reads_fails() {
    let db = db();
    let mut parameters = rhizodb::Parameters::new();
    parameters.insert("y".to_owned(), int(1));
    let query = "RETURN $y AS y";
    assert_eq!(
        db.execute(query, &parameters).unwrap().rows(),
        [vec![int(1)]]
    );
    let error = db.execute(query, &rhizodb::Parameters::new()).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::MissingParameter);
}

#[test]
fn opening_without_create_refuses_an_empty_file_and_leaves_it_empty() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph.rhizodb");
    std::fs::File::create(&path).unwrap();
    let refused = rhizodb::OpenOptions::new()
        .create(false)
        .open(&path)
        .unwrap_err();
    assert_eq!(refused.kind(), rhizodb::ErrorKind::DatabaseNotFound);
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
}
