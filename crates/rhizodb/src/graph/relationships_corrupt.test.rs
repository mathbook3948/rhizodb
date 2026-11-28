use super::*;
use crate::Database;
use crate::ErrorKind;
use crate::Parameters;

fn database_with_relationships() -> Database {
    let db = Database::in_memory().unwrap();
    db.execute(
        "UNWIND range(0, 99) AS i CREATE (:Item {i: i})-[:R {k: i * 1000, s: 'text ' + toString(i)}]->(:Item {i: i + 100})",
        &Parameters::new(),
    )
    .unwrap();
    db
}

fn stored_chunks(db: &Database) -> Vec<(Vec<u8>, Vec<u8>)> {
    db.graph
        .storage()
        .begin_read()
        .unwrap()
        .scan_prefix(Table::RelationshipChunks, &[])
        .unwrap()
}

fn store_chunk(db: &Database, key: &[u8], value: &[u8]) {
    let mut write = db.graph.storage().begin_write().unwrap();
    write.put(Table::RelationshipChunks, key, value).unwrap();
    write.commit().unwrap();
}

fn decoded(key: &[u8], bytes: &[u8]) -> Option<Vec<RelationshipRecord>> {
    ChunkRef::parse(key, bytes)
        .ok()?
        .entries()
        .map(|entry| RelationshipRecord::decode(&entry.ok()?.1).ok())
        .collect()
}

fn silent_damage(key: &[u8], value: &[u8]) -> Vec<u8> {
    let before = decoded(key, value).unwrap();
    (0..value.len())
        .rev()
        .map(|at| {
            let mut damaged = value.to_vec();
            damaged[at] ^= 1;
            damaged
        })
        .find(|damaged| {
            decoded(key, &relationships::with_checksum(key, damaged.clone()))
                .is_some_and(|after| after != before)
        })
        .expect("some damage of one byte decodes as other records")
}

fn kind_of(db: &Database, query: &str) -> Option<ErrorKind> {
    db.execute(query, &Parameters::new())
        .err()
        .map(|error| error.kind())
}

#[test]
fn a_damaged_relationship_chunk_fails_every_read_and_write_and_the_check_as_corrupt_data() {
    let db = database_with_relationships();
    db.check().unwrap();
    let (key, value) = stored_chunks(&db).remove(0);
    let damaged = silent_damage(&key, &value);
    store_chunk(&db, &key, &damaged);

    let corrupt = Some(ErrorKind::CorruptData);
    assert_eq!(kind_of(&db, "MATCH ()-[r:R]->() RETURN r.k"), corrupt);
    assert_eq!(kind_of(&db, "MATCH ()-[r:R]->() RETURN r"), corrupt);
    assert_eq!(
        kind_of(&db, "MATCH (:Item {i: 0})-[r]->() RETURN r.k, r.s"),
        corrupt
    );
    assert_eq!(
        kind_of(&db, "MATCH (:Item {i: 1})-[r]->() SET r.k = 1"),
        corrupt
    );
    assert_eq!(db.check().unwrap_err().kind(), ErrorKind::CorruptData);
    assert_eq!(
        kind_of(&db, "MATCH (:Item {i: 99})-[r]->() RETURN r.k"),
        None,
        "a chunk past the damaged one still reads"
    );

    store_chunk(&db, &key, &value);
    db.check().unwrap();
    assert_eq!(
        db.execute(
            "MATCH (:Item {i: 3})-[r]->() RETURN r.k",
            &Parameters::new()
        )
        .unwrap()
        .rows(),
        [vec![crate::Value::Integer(3000)]]
    );
}

#[test]
fn a_relationship_chunk_stored_under_another_chunks_key_fails_the_check() {
    let db = database_with_relationships();
    let chunks = stored_chunks(&db);
    assert!(chunks.len() > 2);
    store_chunk(&db, &chunks[0].0, &chunks[1].1);

    assert_eq!(db.check().unwrap_err().kind(), ErrorKind::CorruptData);
    assert_eq!(
        kind_of(&db, "MATCH (:Item {i: 0})-[r]->() RETURN r.k"),
        Some(ErrorKind::CorruptData)
    );
}

#[test]
fn a_relationship_chunk_without_its_checksum_fails_as_corrupt_data_in_the_current_format() {
    let db = database_with_relationships();
    let (key, value) = stored_chunks(&db).remove(0);
    store_chunk(
        &db,
        &key,
        &relationships::unsealed(&key, &relationships::stored_records(&key, &value)),
    );

    assert_eq!(db.check().unwrap_err().kind(), ErrorKind::CorruptData);
    assert_eq!(
        kind_of(&db, "MATCH ()-[r:R]->() RETURN r.k"),
        Some(ErrorKind::CorruptData)
    );
}
