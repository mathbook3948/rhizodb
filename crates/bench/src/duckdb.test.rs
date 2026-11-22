use std::fs;

use super::*;

fn open(name: &str) -> (std::path::PathBuf, Box<dyn Instance>) {
    let dir = std::env::temp_dir().join(format!("bench-duckdb-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let instance = DuckdbEngine.open(&dir).unwrap();
    instance.create_schema().unwrap();
    (dir, instance)
}

#[test]
fn placeholders_are_the_dollar_names_each_once_in_order() {
    assert_eq!(
        placeholders("SELECT $b, $a, $b FROM t WHERE x = $a_1"),
        ["b", "a", "a_1"]
    );
    assert!(placeholders("SELECT 1").is_empty());
}

#[test]
fn only_the_parameters_a_statement_names_are_bound() {
    let params = [
        ("id", Param::Int(1)),
        ("unused", Param::Int(2)),
        ("name", Param::Text("x".to_owned())),
    ];

    let bound = named("SELECT $name, $id", &params);

    assert_eq!(
        bound.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
        ["name", "id"]
    );
}

#[test]
fn a_named_parameter_write_is_read_back_by_a_named_parameter_lookup() {
    let (dir, instance) = open("params");
    let mut session = instance.session().unwrap();

    session
        .run(
            &Query::new("INSERT INTO cities(id, name) VALUES ($id, $name)")
                .int("id", 7)
                .text("name", "Oslo"),
        )
        .unwrap();
    let found = session
        .run(&Query::new("SELECT name FROM cities WHERE id = $id").int("id", 7))
        .unwrap();

    assert_eq!(found.rows, 1);
    drop(session);
    instance.close().unwrap();
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn an_embedding_goes_in_as_text_and_comes_back_nearest_first() {
    let (dir, instance) = open("vector");
    let mut session = instance.session().unwrap();
    let mut insert = |id: i64, head: f64| {
        let mut embedding = vec![0.0; 64];
        embedding[0] = head;
        embedding[1] = 1.0;
        session
            .run(
                &Query::new("INSERT INTO persons(id, name, age, bio, embedding) VALUES ($id, 'x', NULL, 'b', CAST($embedding AS FLOAT[64]))")
                    .int("id", id)
                    .floats("embedding", &embedding),
            )
            .unwrap();
    };
    insert(1, 5.0);
    insert(2, 0.1);
    insert(3, 1.0);
    let mut query = vec![0.0; 64];
    query[0] = 0.0;
    query[1] = 1.0;

    let found = session
        .ids(&Query::new(instance.vector_search().unwrap()).floats("vector", &query))
        .unwrap();

    assert_eq!(found, [2, 3, 1]);
    drop(session);
    instance.close().unwrap();
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn a_failed_query_is_an_error() {
    let (dir, instance) = open("failure");
    let mut session = instance.session().unwrap();

    assert!(session.run(&Query::new("SELEC 1")).is_err());
    drop(session);
    instance.close().unwrap();
    let _ = fs::remove_dir_all(dir);
}
