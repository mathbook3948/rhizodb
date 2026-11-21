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
    let path = dir.path().join("graph.redb");
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
    assert_eq!(node.id.as_u64(), 0);
    assert_eq!(
        node.labels.iter().map(String::as_str).collect::<Vec<_>>(),
        ["A", "B"]
    );
    assert_eq!(node.properties["x"], int(1));
    let value = single(&db, "MATCH (n:A) CREATE (n)-[r:R {w: 2}]->(m) RETURN r");
    let Value::Relationship(relationship) = value else {
        panic!("expected a relationship, got {value:?}");
    };
    assert_eq!(relationship.rel_type, "R");
    assert_eq!(relationship.start, node.id);
    assert_ne!(relationship.end, node.id);
    assert_eq!(relationship.properties["w"], int(2));
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
