mod support;

use rhizodb::IndexTarget;
use rhizodb::Value;
use support::*;

fn graph() -> rhizodb::Database {
    let db = db();
    run(
        &db,
        "INSERT (a:N {k: 1}), (b:N {k: 2}), (c:N {k: 3}), (d:M {k: 4}),
         (a)-[:T {w: 1}]->(b), (b)-[:T {w: 2}]->(c), (c)-[:U {w: 3}]->(a),
         (a)-[:T {w: 4}]->(d), (d)-[:U {w: 5}]->(b), (b)-[:T {w: 6}]->(b)",
    );
    db
}

fn sorted(db: &rhizodb::Database, query: &str) -> Vec<Vec<Value>> {
    let mut rows = run(db, query);
    rows.sort_by_key(|row| format!("{row:?}"));
    rows
}

const EDGES: [&str; 8] = ["->", "<-", "-", "<->", "~>", "<~", "-[:T]->", "<-[:U]-"];

#[test]
fn a_pattern_whose_end_has_a_value_matches_as_its_path_form_does() {
    for indexed in [false, true] {
        let db = graph();
        if indexed {
            db.create_property_index("n_k", IndexTarget::Label("N"), "k")
                .unwrap();
        }
        ends_with_values(&db);
    }
}

fn ends_with_values(db: &rhizodb::Database) {
    for edge in EDGES {
        for (end, filter) in [
            ("(z:N {k: 2})", ""),
            ("(z)", "WHERE z.k = 3"),
            ("(z)", "WHERE z.k IN [1, 4]"),
            ("(z:N)", "WHERE z.k >= 2 AND x.k <> z.k"),
        ] {
            let plain = format!("MATCH (x){edge}(y){edge}{end} {filter} RETURN x.k, y.k, z.k");
            let path = format!("MATCH p = (x){edge}(y){edge}{end} {filter} RETURN x.k, y.k, z.k");
            assert_eq!(sorted(db, &plain), sorted(db, &path), "{plain}");
        }
    }
}

#[test]
fn a_pattern_whose_end_is_bound_matches_as_its_path_form_does() {
    let db = graph();
    for edge in EDGES {
        let plain =
            format!("MATCH (z:N) WITH z MATCH (x)-[r]-(y){edge}(z) RETURN x.k, y.k, z.k, r.w");
        let path =
            format!("MATCH (z:N) WITH z MATCH p = (x)-[r]-(y){edge}(z) RETURN x.k, y.k, z.k, r.w");
        assert_eq!(sorted(&db, &plain), sorted(&db, &path), "{plain}");
        let optional = format!("MATCH (z) OPTIONAL MATCH (x:M){edge}(z) RETURN z.k, x.k");
        let optional_path = format!("MATCH (z) OPTIONAL MATCH p = (x:M){edge}(z) RETURN z.k, x.k");
        assert_eq!(
            sorted(&db, &optional),
            sorted(&db, &optional_path),
            "{optional}"
        );
    }
}

#[test]
fn a_pattern_that_reads_its_own_variables_or_repeats_one_keeps_its_meaning() {
    let db = graph();
    let rows = sorted(
        &db,
        "MATCH (x)-[r]->(y)-[s]->(z {k: 1})-[t]->(x) RETURN x.k, y.k",
    );
    assert_eq!(rows, [vec![Value::Integer(2), Value::Integer(3)]]);
    let rows = sorted(&db, "MATCH (x)-[r]->(y {k: x.k + 1}) RETURN x.k, y.k, r.w");
    assert_eq!(
        rows,
        [
            vec![Value::Integer(1), Value::Integer(2), Value::Integer(1)],
            vec![Value::Integer(2), Value::Integer(3), Value::Integer(2)],
        ]
    );
    let rows = sorted(&db, "MATCH (x)-[r]->(x {k: 2}) RETURN x.k, r.w");
    assert_eq!(rows, [vec![Value::Integer(2), Value::Integer(6)]]);
}

#[test]
fn a_filter_tested_while_matching_keeps_the_rows_of_the_filter_tested_after() {
    let db = graph();
    for filter in [
        "x.k > 1 AND r.w < 6 AND y.k <> 2",
        "r.w IN [1, 2, 4] AND z.k >= 2 AND x <> z",
        "x.k = 1 OR s.w > 2",
        "NOT y.k IS NULL AND id(x) <> id(z) AND s.w <= 5",
        "x.k STARTS WITH 'a' OR y.k < 3",
    ] {
        let early =
            format!("MATCH (x)-[r]-(y)-[s]->(z) WHERE {filter} RETURN x.k, y.k, z.k, r.w, s.w");
        let late = format!(
            "MATCH (x)-[r]-(y)-[s]->(z) WHERE {filter} AND x.k + 0 = x.k RETURN x.k, y.k, z.k, r.w, s.w"
        );
        assert_eq!(sorted(&db, &early), sorted(&db, &late), "{filter}");
    }
    let rows = db.execute(
        "MATCH (x)-[r]->(y) WHERE r.w > 100 AND x.k - 'a' = 1 RETURN x",
        &rhizodb::Parameters::new(),
    );
    assert!(rows.is_err());
}

#[test]
fn a_query_reads_the_relationships_and_properties_it_wrote_itself() {
    let db = graph();
    let rows = run(
        &db,
        "UNWIND range(1, 4) AS i CALL (i) { MATCH (a:N {k: 1})-[:T]->(x) WITH a, i, count(x) AS n CREATE (a)-[:T]->(:N {k: 10 + i}) RETURN n } RETURN i, n ORDER BY i",
    );
    let counts: Vec<i64> = rows
        .iter()
        .map(|row| match row.as_slice() {
            [_, Value::Integer(n)] => *n,
            other => panic!("expected two integers, got {other:?}"),
        })
        .collect();
    assert_eq!(counts, [2, 3, 4, 5]);
    let rows = run(
        &db,
        "UNWIND range(1, 4) AS i CALL (i) { MATCH (:N {k: 1})-[r:T]->(:N {k: 2}) WITH r, r.w AS w SET r.w = w + 1 RETURN w } RETURN w ORDER BY w",
    );
    assert_eq!(
        rows,
        (1..5).map(|w| vec![Value::Integer(w)]).collect::<Vec<_>>()
    );
}
