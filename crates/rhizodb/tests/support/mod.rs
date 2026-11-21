#![allow(
    dead_code,
    reason = "each test binary uses a different subset of the helpers"
)]

use std::collections::BTreeMap;

use rhizodb::Database;
use rhizodb::ErrorKind;
use rhizodb::Parameters;
use rhizodb::Value;

pub fn db() -> Database {
    Database::in_memory().unwrap()
}

pub fn run(db: &Database, query: &str) -> Vec<Vec<Value>> {
    match db.execute(query, &Parameters::new()) {
        Ok(result) => result.rows().to_vec(),
        Err(error) => panic!("{query}: {error} ({:?})", error.kind()),
    }
}

pub fn columns(db: &Database, query: &str) -> Vec<String> {
    db.execute(query, &Parameters::new())
        .unwrap()
        .columns()
        .to_vec()
}

pub fn single(db: &Database, query: &str) -> Value {
    let rows = run(db, query);
    assert_eq!(rows.len(), 1, "{query} returned {rows:?}");
    assert_eq!(rows[0].len(), 1, "{query} returned {rows:?}");
    rows[0][0].clone()
}

pub fn error_kind(db: &Database, query: &str) -> ErrorKind {
    match db.execute(query, &Parameters::new()) {
        Ok(result) => panic!("{query} succeeded with {:?}", result.rows()),
        Err(error) => error.kind(),
    }
}

pub fn int(value: i64) -> Value {
    Value::Integer(value)
}

pub fn float(value: f64) -> Value {
    Value::Float(value)
}

pub fn string(value: &str) -> Value {
    Value::String(value.to_owned())
}

pub fn boolean(value: bool) -> Value {
    Value::Boolean(value)
}

pub fn list(items: impl IntoIterator<Item = Value>) -> Value {
    Value::List(items.into_iter().collect())
}

pub fn map(entries: &[(&str, Value)]) -> Value {
    Value::Map(
        entries
            .iter()
            .map(|(key, value)| ((*key).to_owned(), value.clone()))
            .collect::<BTreeMap<_, _>>(),
    )
}

pub fn ints(rows: &[Vec<Value>]) -> Vec<i64> {
    rows.iter()
        .map(|row| match row.as_slice() {
            [Value::Integer(value)] => *value,
            other => panic!("expected one integer, got {other:?}"),
        })
        .collect()
}
