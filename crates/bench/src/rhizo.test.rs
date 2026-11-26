use std::fs;

use super::*;

fn open(name: &str) -> (std::path::PathBuf, Box<dyn Instance>) {
    let dir = std::env::temp_dir().join(format!("bench-rhizo-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let instance = RhizoEngine::default().open(&dir).unwrap();
    (dir, instance)
}

#[test]
fn a_parameterised_write_is_read_back_by_a_parameterised_lookup() {
    let (dir, instance) = open("params");
    let mut session = instance.session().unwrap();

    session
        .run(
            &Query::new("CREATE (:Person {id: $id, name: $name})")
                .int("id", 7)
                .text("name", "Ada"),
        )
        .unwrap();
    let found = session
        .run(&Query::new("MATCH (p:Person {id: $id}) RETURN p.name").int("id", 7))
        .unwrap();
    let missing = session
        .run(&Query::new("MATCH (p:Person {id: $id}) RETURN p.name").int("id", 8))
        .unwrap();

    assert_eq!(found.rows, 1);
    assert_eq!(missing.rows, 0);
    drop(session);
    drop(instance);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn a_transaction_hides_its_writes_until_commit() {
    let (dir, instance) = open("transaction");
    let mut writer = instance.session().unwrap();
    let mut reader = instance.session().unwrap();
    let count = Query::new("MATCH (p:Person) RETURN count(*)");

    writer.begin().unwrap();
    writer.run(&Query::new("CREATE (:Person {id: 1})")).unwrap();
    let before = reader.run(&count).unwrap();
    writer.commit().unwrap();
    let after = reader.run(&count).unwrap();

    assert_ne!(before, after);
    drop((writer, reader));
    drop(instance);
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
