use std::fs;

use super::*;

fn open(name: &str) -> (std::path::PathBuf, Box<dyn Instance>) {
    let dir = std::env::temp_dir().join(format!("bench-cozo-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let instance = CozoEngine.open(&dir).unwrap();
    instance.create_schema().unwrap();
    (dir, instance)
}

#[test]
fn a_parameterised_write_is_read_back_by_a_parameterised_lookup() {
    let (dir, instance) = open("params");
    let mut session = instance.session().unwrap();

    session
        .run(
            &Query::new("?[id, name] <- [[$id, $name]] :put cities {id => name}")
                .int("id", 7)
                .text("name", "Oslo"),
        )
        .unwrap();
    let found = session
        .run(&Query::new("?[name] := *cities{id: $id, name}").int("id", 7))
        .unwrap();
    let missing = session
        .run(&Query::new("?[name] := *cities{id: $id, name}").int("id", 8))
        .unwrap();

    assert_eq!((found.rows, missing.rows), (1, 0));
    drop(session);
    drop(instance);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn a_transaction_applies_its_writes_at_commit() {
    let (dir, instance) = open("transaction");
    let mut writer = instance.session().unwrap();
    let mut reader = instance.session().unwrap();
    let count = Query::new("?[count(id)] := *cities{id}");

    writer.begin().unwrap();
    writer
        .run(&Query::new(
            "?[id, name] <- [[1, 'a']] :put cities {id => name}",
        ))
        .unwrap();
    writer.commit().unwrap();

    assert_eq!(reader.ids(&count).unwrap(), [1]);
    drop((writer, reader));
    drop(instance);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn a_failed_query_is_an_error() {
    let (dir, instance) = open("failure");
    let mut session = instance.session().unwrap();

    assert!(session.run(&Query::new("?[x] := *missing{x}")).is_err());
    drop(session);
    drop(instance);
    let _ = fs::remove_dir_all(dir);
}
