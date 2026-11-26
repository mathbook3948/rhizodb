//! A match begins at the node of its pattern that the row binds or that a lookup serves, and
//! still gives the rows, paths, and errors of the pattern as written.

mod support;

use rhizodb::Database;
use rhizodb::ErrorKind;
use rhizodb::IndexTarget;
use rhizodb::Parameters;
use rhizodb::Value;
use support::*;

/// A chain `a -> b -> c` of `T` relationships, a second `T` from `a` to `b`, and a `Q` node that
/// no relationship touches.
fn chain() -> Database {
    let db = db();
    run(
        &db,
        "INSERT (a:P {name: 'a'}), (b:P {name: 'b'}), (c:P {name: 'c'}), (:Q {k: 0}), \
         (a)-[:T {name: 'ab'}]->(b), (b)-[:T {name: 'bc'}]->(c), (a)-[:T {name: 'ab2'}]->(b)",
    );
    db
}

fn rows(db: &Database, query: &str, parameters: &Parameters) -> Result<Vec<Vec<Value>>, ErrorKind> {
    db.execute(query, parameters)
        .map(|result| result.rows().to_vec())
        .map_err(|error| error.kind())
}

fn strings(items: &[&str]) -> Value {
    list(items.iter().map(|item| string(item)))
}

#[test]
fn a_path_bound_at_its_end_keeps_its_written_order() {
    let db = chain();
    for query in [
        "MATCH (c:P {name: 'c'}) MATCH p = (a)-[:T]->(b)-[:T]->(c) \
         RETURN [n IN nodes(p) | n.name], [r IN relationships(p) | r.name] AS rs ORDER BY rs",
        "MATCH (b:P {name: 'b'}) MATCH p = (a)-[:T]->(b)-[:T]->(c) \
         RETURN [n IN nodes(p) | n.name], [r IN relationships(p) | r.name] AS rs ORDER BY rs",
        "MATCH (c:P {name: 'c'}) MATCH p = (a)-[:T*2]->(c) \
         RETURN [n IN nodes(p) | n.name], [r IN relationships(p) | r.name] AS rs ORDER BY rs",
    ] {
        assert_eq!(
            run(&db, query),
            [
                vec![strings(&["a", "b", "c"]), strings(&["ab", "bc"])],
                vec![strings(&["a", "b", "c"]), strings(&["ab2", "bc"])],
            ],
            "{query}"
        );
    }
}

#[test]
fn a_variable_length_list_bound_at_its_end_keeps_its_written_order() {
    let db = chain();
    let query = "MATCH (c:P {name: 'c'}) MATCH (a)-[rs:T*1..2]->(c) \
                 RETURN a.name AS a, [r IN rs | r.name] AS rs ORDER BY a, rs";
    assert_eq!(
        run(&db, query),
        [
            vec![string("a"), strings(&["ab", "bc"])],
            vec![string("a"), strings(&["ab2", "bc"])],
            vec![string("b"), strings(&["bc"])],
        ]
    );
}

#[test]
fn a_match_from_a_bound_later_node_keeps_direction_and_multiplicity() {
    let db = chain();
    assert_eq!(
        run(
            &db,
            "MATCH (b:P {name: 'b'}) MATCH (x)-[r:T]->(b) RETURN x.name, r.name ORDER BY r.name"
        ),
        [
            vec![string("a"), string("ab")],
            vec![string("a"), string("ab2")],
        ]
    );
    assert_eq!(
        run(
            &db,
            "MATCH (b:P {name: 'b'}) OPTIONAL MATCH (x)-[:T]->(y)-[:T]->(b) RETURN x, y"
        ),
        [vec![Value::Null, Value::Null]]
    );
    assert_eq!(
        single(
            &db,
            "MATCH (b:P {name: 'b'}) MATCH (x)-[r]-(b)-[s]-(y) RETURN count(*)"
        ),
        int(6)
    );
}

#[test]
fn a_bound_later_node_starts_the_match() {
    let db = chain();
    let trap = Parameters::new().with("zero", 0i64);
    let query = "MATCH (q:Q) MATCH (a:P {name: 1 / $zero})-[:T]->(q) RETURN a";
    assert_eq!(rows(&db, query, &trap), Ok(Vec::new()));
    let query = "MATCH (a:P {name: 1 / $zero})-[:T]->(q:Q) RETURN a";
    assert_eq!(rows(&db, query, &trap), Err(ErrorKind::DivisionByZero));
}

#[test]
fn a_later_node_that_an_index_serves_starts_the_match() {
    let db = chain();
    let trap = Parameters::new().with("zero", 0i64);
    let query = "MATCH (a:P {k: 1 / $zero})-[:T]->(b:P {name: 'z'}) RETURN a";
    assert_eq!(rows(&db, query, &trap), Err(ErrorKind::DivisionByZero));
    db.create_property_index("p_name", IndexTarget::Label("P"), "name")
        .unwrap();
    assert_eq!(rows(&db, query, &trap), Ok(Vec::new()));
    let query =
        "MATCH (a)-[r:T]->(b:P) WHERE b.name IN ['b'] RETURN a.name, r.name ORDER BY r.name";
    assert_eq!(
        run(&db, query),
        [
            vec![string("a"), string("ab")],
            vec![string("a"), string("ab2")],
        ]
    );
}
