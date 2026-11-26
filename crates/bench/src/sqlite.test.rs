use std::fs;

use super::*;
use crate::rhizo::RhizoEngine;

fn open(name: &str) -> (std::path::PathBuf, Box<dyn Instance>) {
    let dir = std::env::temp_dir().join(format!("bench-sqlite-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let instance = SqliteEngine.open(&dir).unwrap();
    instance.create_schema().unwrap();
    (dir, instance)
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
    let missing = session
        .run(&Query::new("SELECT name FROM cities WHERE id = $id").int("id", 8))
        .unwrap();

    assert_eq!((found.rows, missing.rows), (1, 0));
    drop(session);
    instance.close().unwrap();
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn a_rows_parameter_runs_the_statement_once_per_row() {
    let (dir, instance) = open("rows");
    let mut session = instance.session().unwrap();
    let rows = vec![
        vec![("id", Param::Int(1)), ("name", Param::Text("a".to_owned()))],
        vec![("id", Param::Int(2)), ("name", Param::Text("b".to_owned()))],
    ];

    session
        .run(&Query::new("INSERT INTO cities(id, name) VALUES ($id, $name)").rows("rows", rows))
        .unwrap();
    let count = session
        .run(&Query::new("SELECT count(*) FROM cities"))
        .unwrap();

    assert_eq!(count.rows, 1);
    assert_eq!(
        session
            .ids(&Query::new("SELECT count(*) FROM cities"))
            .unwrap(),
        [2]
    );
    drop(session);
    instance.close().unwrap();
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn sqlite_and_rhizodb_fingerprint_the_same_row_alike() {
    let dir = std::env::temp_dir().join(format!("bench-sqlite-both-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("r")).unwrap();
    fs::create_dir_all(dir.join("s")).unwrap();
    let rhizo = RhizoEngine::default().open(&dir.join("r")).unwrap();
    let sqlite = SqliteEngine.open(&dir.join("s")).unwrap();

    let from_rhizo = rhizo
        .session()
        .unwrap()
        .run(&Query::new("RETURN 3, 2.5, 'x', null"))
        .unwrap();
    let from_sqlite = sqlite
        .session()
        .unwrap()
        .run(&Query::new("SELECT 3, 2.5, 'x', NULL"))
        .unwrap();

    assert_eq!(from_rhizo, from_sqlite);
    drop(rhizo);
    sqlite.close().unwrap();
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn a_script_with_several_statements_runs_in_one_call() {
    let (dir, instance) = open("script");
    let mut session = instance.session().unwrap();

    session
        .run(&Query::new(
            "BEGIN; INSERT INTO cities VALUES (1, 'a'); INSERT INTO cities VALUES (2, 'b'); COMMIT;",
        ))
        .unwrap();

    assert_eq!(
        session
            .ids(&Query::new("SELECT count(*) FROM cities"))
            .unwrap(),
        [2]
    );
    drop(session);
    instance.close().unwrap();
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn keyword_search_finds_the_rows_whose_bio_holds_the_word() {
    let (dir, instance) = open("keyword");
    let mut session = instance.session().unwrap();
    for (id, bio) in [
        (1, "graph node edge"),
        (2, "keyword search"),
        (3, "graph search"),
    ] {
        session
            .run(
                &Query::new("INSERT INTO persons(id, name, age, bio, embedding) VALUES ($id, 'x', NULL, $bio, NULL)")
                    .int("id", id)
                    .text("bio", bio),
            )
            .unwrap();
    }
    assert_eq!(instance.create_keyword_index().unwrap(), Built::Index);

    let mut found = session
        .ids(&Query::new(instance.keyword_search().unwrap()).text("term", "graph"))
        .unwrap();
    found.sort_unstable();

    assert_eq!(found, [1, 3]);
    assert!(matches!(
        instance.create_vector_index(),
        Err(EngineError::Unsupported(_))
    ));
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
