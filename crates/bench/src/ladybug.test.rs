use std::fs;

use super::*;
use crate::rhizo::RhizoEngine;

fn open(name: &str) -> (std::path::PathBuf, Box<dyn Instance>) {
    let dir = std::env::temp_dir().join(format!("bench-ladybug-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let instance = LadybugEngine::default().open(&dir).unwrap();
    instance.create_schema().unwrap();
    (dir, instance)
}

#[test]
fn a_parameterised_write_is_read_back_by_a_parameterised_lookup() {
    let (dir, instance) = open("params");
    let mut session = instance.session().unwrap();

    session
        .run(
            &Query::new("CREATE (:City {id: $id, name: $name})")
                .int("id", 7)
                .text("name", "Oslo"),
        )
        .unwrap();
    let found = session
        .run(&Query::new("MATCH (c:City {id: $id}) RETURN c.name").int("id", 7))
        .unwrap();

    assert_eq!(found.rows, 1);
    drop(session);
    drop(instance);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn both_engines_fingerprint_the_same_row_alike() {
    let dir = std::env::temp_dir().join(format!("bench-both-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("r")).unwrap();
    fs::create_dir_all(dir.join("l")).unwrap();
    let rhizo = RhizoEngine::default().open(&dir.join("r")).unwrap();
    let ladybug = LadybugEngine::default().open(&dir.join("l")).unwrap();
    let query = Query::new("RETURN 3, 2.5, 'x', true, [1, 2], null");

    let from_rhizo = rhizo.session().unwrap().run(&query).unwrap();
    let from_ladybug = ladybug.session().unwrap().run(&query).unwrap();

    assert_eq!(from_rhizo, from_ladybug);
    drop((rhizo, ladybug));
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn a_failed_query_is_an_error() {
    let (dir, instance) = open("failure");
    let mut session = instance.session().unwrap();

    assert!(session.run(&Query::new("MATCH (")).is_err());
    drop(session);
    drop(instance);
    let _ = fs::remove_dir_all(dir);
}
