use super::*;

#[test]
fn every_returned_column_is_named_by_its_position() {
    assert_eq!(
        alias_columns("MATCH (p:Person) RETURN p.id, p.age ORDER BY p.age DESC LIMIT 10"),
        "MATCH (p:Person) RETURN p.id AS c0, p.age AS c1 ORDER BY p.age DESC LIMIT 10"
    );
    assert_eq!(
        alias_columns("MATCH (p:Person) RETURN DISTINCT p.name"),
        "MATCH (p:Person) RETURN DISTINCT p.name AS c0"
    );
    assert_eq!(
        alias_columns("MATCH (p)-[:LIVES_IN]->(c) RETURN c.name, count(p), avg(p.age)"),
        "MATCH (p)-[:LIVES_IN]->(c) RETURN c.name AS c0, count(p) AS c1, avg(p.age) AS c2"
    );
}

#[test]
fn an_existing_alias_is_replaced_and_commas_inside_brackets_are_not_split() {
    assert_eq!(
        alias_columns(
            "CALL f($x) YIELD node, score RETURN node.id AS id, score ORDER BY score DESC LIMIT 10"
        ),
        "CALL f($x) YIELD node, score RETURN node.id AS c0, score AS c1 ORDER BY score DESC LIMIT 10"
    );
    assert_eq!(
        alias_columns("RETURN 3, 2.5, 'x, y', [1, 2], {a: 1, b: 2}"),
        "RETURN 3 AS c0, 2.5 AS c1, 'x, y' AS c2, [1, 2] AS c3, {a: 1, b: 2} AS c4"
    );
}

#[test]
fn a_statement_without_return_is_left_alone() {
    let text = "UNWIND $rows AS r CREATE (:City {id: r.id})";

    assert_eq!(alias_columns(text), text);
}

#[test]
fn a_server_without_a_url_is_unsupported_not_a_failure() {
    let engine = BoltEngine::new(Flavor::Neo4j, None);

    assert!(matches!(
        engine.open(Path::new(".")),
        Err(EngineError::Unsupported(_))
    ));
}
