use std::cell::Cell;

use crate::Database;
use crate::ErrorKind;
use crate::Parameters;
use crate::Value;
use crate::graph::counted;

const EDGES: i64 = 40;

thread_local! {
    static EVALUATED: Cell<usize> = const { Cell::new(0) };
}

pub(crate) fn evaluated() {
    EVALUATED.with(|evaluated| evaluated.set(evaluated.get() + 1));
}

fn evaluations(run: impl FnOnce()) -> usize {
    let before = EVALUATED.with(Cell::get);
    run();
    EVALUATED.with(Cell::get) - before
}

fn graph() -> Database {
    graph_of(EDGES)
}

fn graph_of(edges: i64) -> Database {
    let db = Database::in_memory().unwrap();
    db.execute(
        "UNWIND range(0, $edges - 1) AS i \
         INSERT (:A {k: i % 4})-[:R {w: i % 5, name: 'r' + toString(i), list: [i, 1]}]->\
         (:B {k: i % 4, active: i % 2 = 0})",
        &Parameters::from([("edges", edges)]),
    )
    .unwrap();
    db
}

fn count(db: &Database, query: &str, parameters: &Parameters) -> i64 {
    match db.execute(query, parameters).unwrap().rows() {
        [row] => match row.as_slice() {
            [Value::Integer(count)] => *count,
            other => panic!("expected one count, got {other:?}"),
        },
        other => panic!("expected one row, got {other:?}"),
    }
}

#[test]
fn relationship_maps_match_as_before() {
    let db = graph();
    let none = Parameters::new();
    let cases: [(&str, Parameters, i64); 10] = [
        ("MATCH ()-[r:R {w: 1}]->() RETURN count(*)", none.clone(), 8),
        (
            "MATCH ()-[r:R {w: $w}]->() RETURN count(*)",
            Parameters::from([("w", 1)]),
            8,
        ),
        (
            "MATCH ()-[r:R {w: 1, name: 'r6'}]->() RETURN count(*)",
            none.clone(),
            1,
        ),
        (
            "MATCH ()-[r:R {w: 1, name: 'r7'}]->() RETURN count(*)",
            none.clone(),
            0,
        ),
        (
            "MATCH ()-[r:R {list: [3, 1]}]->() RETURN count(*)",
            none.clone(),
            1,
        ),
        (
            "MATCH ()-[r:R {w: 1.0}]->() RETURN count(*)",
            none.clone(),
            8,
        ),
        (
            "MATCH ()-[r:R {w: null}]->() RETURN count(*)",
            none.clone(),
            0,
        ),
        (
            "MATCH ()-[r:R {absent: 1}]->() RETURN count(*)",
            none.clone(),
            0,
        ),
        ("MATCH ()-[r:R {}]->() RETURN count(*)", none.clone(), EDGES),
        (
            "MATCH (a:A)-[r:R {w: a.k}]->() RETURN count(*)",
            none.clone(),
            8,
        ),
    ];
    for (query, parameters, expected) in cases {
        assert_eq!(count(&db, query, &parameters), expected, "{query}");
    }
}

#[test]
fn node_maps_match_as_before() {
    let db = graph();
    let none = Parameters::new();
    let cases: [(&str, Parameters, i64); 6] = [
        (
            "MATCH (:A)-[:R]->(b {active: true}) RETURN count(*)",
            none.clone(),
            20,
        ),
        (
            "MATCH (:A)-[:R]->(b {active: $t, k: 2}) RETURN count(*)",
            Parameters::from([("t", true)]),
            10,
        ),
        (
            "MATCH (:A)-[:R]->(b {k: 2, active: false}) RETURN count(*)",
            none.clone(),
            0,
        ),
        (
            "MATCH (a:A)-[:R]->(b {k: a.k}) RETURN count(*)",
            none.clone(),
            EDGES,
        ),
        (
            "MATCH (:A)-[:R]->(b {absent: null}) RETURN count(*)",
            none.clone(),
            0,
        ),
        (
            "MATCH (:A)-[:R*1..2]->(b {active: true}) RETURN count(*)",
            none.clone(),
            20,
        ),
    ];
    for (query, parameters, expected) in cases {
        assert_eq!(count(&db, query, &parameters), expected, "{query}");
    }
}

#[test]
fn a_map_that_fails_fails_as_before() {
    let db = graph();
    for (query, parameters) in [
        (
            "MATCH (:A)-[:R]->(b {k: 1 / $zero}) RETURN count(*)",
            Parameters::from([("zero", 0)]),
        ),
        (
            "MATCH (a:A)-[:R]->(b {k: a.k / 0}) RETURN count(*)",
            Parameters::new(),
        ),
        (
            "MATCH ()-[r:R {w: 1 / $zero}]->() RETURN count(*)",
            Parameters::from([("zero", 0)]),
        ),
    ] {
        let error = db.execute(query, &parameters).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::DivisionByZero, "{query}");
    }
}

#[test]
fn a_relationship_map_reads_only_its_keys() {
    let db = graph();
    for query in [
        "MATCH ()-[r:R {w: 1}]->() RETURN count(*)",
        "MATCH ()-[r:R {w: 1, name: 'r6'}]->() RETURN count(*)",
        "MATCH (a:A)-[r:R {w: a.k}]->() RETURN count(*)",
    ] {
        let (_, reads) = counted(|| count(&db, query, &Parameters::new()));
        assert_eq!(reads.relationship_decodes, 0, "{query}");
    }
}

#[test]
fn a_map_looks_its_keys_up_as_often_however_many_elements_it_tests() {
    let small = graph_of(EDGES);
    let large = graph_of(4 * EDGES);
    for query in [
        "MATCH ()-[r:R {w: 1, name: 'r6'}]->() RETURN count(*)",
        "MATCH ()-[r:R {absent: 1}]->() RETURN count(*)",
        "MATCH (:A)-[:R]->(b {active: true, k: 2}) RETURN count(*)",
        "MATCH (a:A)-[r:R {w: a.k}]->(b {k: a.k}) RETURN count(*)",
    ] {
        let lookups = |db: &Database| {
            counted(|| count(db, query, &Parameters::new()))
                .1
                .token_lookups
        };
        assert_eq!(lookups(&small), lookups(&large), "{query}");
    }
}

#[test]
fn a_map_no_row_decides_is_evaluated_once_per_search() {
    let small = graph_of(EDGES);
    let large = graph_of(4 * EDGES);
    let parameters = Parameters::from([("w", 1)]);
    for (query, fixed) in [
        (
            "MATCH ()-[r:R {w: 1, list: [1, 2]}]->() RETURN count(*)",
            true,
        ),
        ("MATCH ()-[r:R {w: $w}]->() RETURN count(*)", true),
        (
            "MATCH (:A)-[:R]->(b {active: true, k: {a: [$w]}.a[0]}) RETURN count(*)",
            false,
        ),
        (
            "MATCH (:A)-[:R*1..2]->(b {active: true}) RETURN count(*)",
            true,
        ),
        ("MATCH (a:A)-[r:R {w: a.k}]->() RETURN count(*)", false),
    ] {
        let evaluated = |db: &Database| evaluations(|| _ = count(db, query, &parameters));
        let (small, large) = (evaluated(&small), evaluated(&large));
        assert_eq!(small == large, fixed, "{query}: {small} then {large}");
    }
}
