use lbug::Database;
use lbug::SystemConfig;

use super::*;

fn with_connection(test: impl FnOnce(&Connection<'_>)) {
    let database = Database::in_memory(SystemConfig::default()).expect("in-memory database");
    let connection = Connection::new(&database).expect("connection");
    test(&connection);
}

fn only_row(connection: &Connection<'_>, query: &str) -> Vec<Value> {
    let rows = execute(connection, query).expect(query);
    assert_eq!(rows.rows.len(), 1, "{query}");
    rows.rows.into_iter().next().unwrap_or_default()
}

#[test]
fn scalars_and_containers_convert_to_tck_values() {
    with_connection(|connection| {
        let row = only_row(
            connection,
            "RETURN 1 AS i, 1.5 AS f, 'x' AS s, true AS b, NULL AS n, [1, 2] AS l, {k: 1} AS m",
        );

        assert_eq!(
            row,
            vec![
                Value::Integer(1),
                Value::Float(1.5),
                Value::String("x".into()),
                Value::Boolean(true),
                Value::Null,
                Value::List(vec![Value::Integer(1), Value::Integer(2)]),
                Value::Map([("k".to_string(), Value::Integer(1))].into_iter().collect()),
            ]
        );
    });
}

#[test]
fn elements_keep_their_label_and_only_the_properties_they_have() {
    with_connection(|connection| {
        for statement in [
            "CREATE NODE TABLE P(id INT64 PRIMARY KEY, age INT64)",
            "CREATE REL TABLE R(FROM P TO P, w INT64)",
            "CREATE (:P {id: 1})",
            "MATCH (a:P) CREATE (a)-[:R {w: 2}]->(a)",
        ] {
            execute(connection, statement).expect(statement);
        }

        let row = only_row(connection, "MATCH (a)-[r]->() RETURN a, r");

        assert_eq!(
            row,
            vec![
                Value::Node(Node {
                    labels: ["P".to_string()].into_iter().collect(),
                    properties: [("id".to_string(), Value::Integer(1))]
                        .into_iter()
                        .collect(),
                }),
                Value::Relationship(Relationship {
                    rel_type: "R".into(),
                    properties: [("w".to_string(), Value::Integer(2))].into_iter().collect(),
                }),
            ]
        );
    });
}

#[test]
fn a_failing_query_reports_a_ladybug_error() {
    with_connection(|connection| {
        let error = execute(connection, "RETURN x").expect_err("unbound variable");

        assert!(matches!(error, QueryError::Raised { kind, .. } if kind == LADYBUG_ERROR));
    });
}

#[test]
fn source_paths_lose_their_machine_specific_directories() {
    assert_eq!(
        without_source_paths(
            "Assertion failed in file \"/root/.cargo/registry/lbug-0.22.3/lbug-src/src/a.h\" on line 3"
        ),
        "Assertion failed in file \"lbug-src/src/a.h\" on line 3"
    );
    assert_eq!(without_source_paths("Parser exception"), "Parser exception");
}
