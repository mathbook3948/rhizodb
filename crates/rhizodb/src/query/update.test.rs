use crate::Database;
use crate::Parameters;
use crate::Value;
use crate::graph::counted;

const NODES: i64 = 50;

fn graph_of(nodes: i64) -> Database {
    let db = Database::in_memory().unwrap();
    db.execute(
        "UNWIND range(0, $nodes - 1) AS i INSERT (:P {k: i, a: 1})-[:R]->(:Q {k: i})",
        &Parameters::from([("nodes", nodes)]),
    )
    .unwrap();
    db
}

fn rows(db: &Database, query: &str) -> Vec<Vec<Value>> {
    let mut tx = db.begin_write().unwrap();
    let rows = tx
        .execute(query, &Parameters::new())
        .unwrap()
        .rows()
        .to_vec();
    tx.rollback().unwrap();
    rows
}

fn values(items: &[Value]) -> Vec<Vec<Value>> {
    vec![items.to_vec()]
}

#[test]
fn an_update_reads_node_meta_and_tokens_once_per_query() {
    let small = graph_of(NODES);
    let large = graph_of(4 * NODES);
    for (query, absent) in [
        (
            "MATCH (n:P) SET n.visits = coalesce(n.visits, 0) + 1",
            false,
        ),
        ("MATCH (n:P) SET n.b = n.a + 1, n:Seen", false),
        ("MATCH (n:P) SET n += {c: n.a} REMOVE n.a, n:P", false),
        ("MATCH (n:P) SET n.c = n.absent", true),
        ("MATCH (n:P) REMOVE n.absent", true),
    ] {
        let reads = |db: &Database| counted(|| rows(db, query)).1;
        let (small, large) = (reads(&small), reads(&large));
        assert_eq!(small.meta_loads, large.meta_loads, "{query}");
        if !absent {
            assert_eq!(small.token_lookups, large.token_lookups, "{query}");
        }
    }
}

#[test]
fn a_read_after_a_write_in_the_same_clause_sees_the_write() {
    let db = graph_of(NODES);
    let cases: [(&str, Vec<Vec<Value>>); 4] = [
        (
            "MATCH (n:P {k: 0}) SET n.before = n.a, n.a = 5, n.after = n.a \
             RETURN n.before, n.after",
            values(&[Value::Integer(1), Value::Integer(5)]),
        ),
        (
            "MATCH (n:P {k: 0}) SET n.fresh_before = n.fresh, n.fresh = 2, n.fresh_after = n.fresh \
             RETURN n.fresh_before, n.fresh_after",
            values(&[Value::Null, Value::Integer(2)]),
        ),
        (
            "MATCH (n:P {k: 0}) SET n.before = n:Done, n:Done, n.after = n:Done \
             RETURN n.before, n.after",
            values(&[Value::Boolean(false), Value::Boolean(true)]),
        ),
        (
            "MATCH (n:P {k: 0}) UNWIND range(1, 3) AS i SET n.total = coalesce(n.total, n.a) + i \
             RETURN DISTINCT n.total",
            values(&[Value::Integer(7)]),
        ),
    ];
    for (query, expected) in cases {
        assert_eq!(rows(&db, query), expected, "{query}");
    }
}

#[test]
fn a_read_after_a_write_in_a_later_clause_sees_the_write() {
    let db = graph_of(NODES);
    let cases: [(&str, Vec<Vec<Value>>); 5] = [
        (
            "MATCH (n:P {k: 0}) WITH n, n.a AS old REMOVE n.a RETURN old, n.a",
            values(&[Value::Integer(1), Value::Null]),
        ),
        (
            "MATCH (n:P {k: 0}) WITH n, n:Gone AS before SET n:Gone REMOVE n:P \
             RETURN before, n:Gone, n:P",
            values(&[
                Value::Boolean(false),
                Value::Boolean(true),
                Value::Boolean(false),
            ]),
        ),
        (
            "MATCH (a:P {k: 0})-[r:R]->() DELETE r WITH a MATCH (a)-[:R]->(x) RETURN count(x)",
            values(&[Value::Integer(0)]),
        ),
        (
            "MATCH (a:P {k: 0})-[:R]->(x) WITH a, count(x) AS before \
             CREATE (a)-[:R]->(:Q) WITH a, before MATCH (a)-[:R]->(y) RETURN before, count(y)",
            values(&[Value::Integer(1), Value::Integer(2)]),
        ),
        (
            "MATCH (n:P) WITH count(n) AS before CREATE (:P {k: -1}) \
             WITH before MATCH (m:P) RETURN before, count(m)",
            values(&[Value::Integer(NODES), Value::Integer(NODES + 1)]),
        ),
    ];
    for (query, expected) in cases {
        assert_eq!(rows(&db, query), expected, "{query}");
    }
}

#[test]
fn a_node_deleted_earlier_in_the_query_reads_as_before() {
    let db = graph_of(NODES);
    let mut tx = db.begin_write().unwrap();
    let error = tx
        .execute(
            "MATCH (n:P {k: 0}) DETACH DELETE n WITH n SET n.a = n.a + 1",
            &Parameters::new(),
        )
        .unwrap_err();
    assert_eq!(error.kind(), crate::ErrorKind::DeletedEntityAccess);
}
