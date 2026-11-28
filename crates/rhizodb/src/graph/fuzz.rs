use std::collections::BTreeMap;

use crate::Database;
use crate::result::Parameters;
use crate::storage::Entry;
use crate::storage::ReadTransaction;
use crate::storage::Storage;
use crate::storage::Table;
use crate::storage::WriteTransaction;

use super::RelationshipId;
use super::RelationshipRecord;

const SETUP: &str = "UNWIND range(0, 79) AS i INSERT (:A {i: i})-[:R {w: i, s: 'text ' + toString(i), l: [i, -i]}]->(:B)";

fn damage(original: &[u8], edits: &[u8], entries: &[Entry]) -> Vec<u8> {
    let mut damaged = original.to_vec();
    for edit in edits.chunks(3) {
        let [kind, value, at] = edit else { break };
        let at = usize::from(*at) % (damaged.len() + 1);
        match kind % 4 {
            0 => {
                if let Some(byte) = damaged.get_mut(at) {
                    *byte ^= value;
                }
            }
            1 => damaged.truncate(at),
            2 => damaged.insert(at, *value),
            _ => damaged.clone_from(&entries[usize::from(*value) % entries.len()].1),
        }
    }
    damaged
}

/// Damages the stored relationship chunks of a small graph by `bytes`, writing them past the
/// storage engine's own checks, and reads every relationship. The first byte picks the chunk,
/// and each three bytes after it flip, cut, or insert a byte, or put another chunk in its place.
/// Fails when a changed chunk reads without an error or passes [`Database::check`].
pub(crate) fn read_damaged_relationship_chunks(bytes: &[u8]) -> Result<(), String> {
    let fail = |error: crate::Error| error.to_string();
    let db = Database::in_memory().map_err(fail)?;
    db.execute(SETUP, &Parameters::new()).map_err(fail)?;
    db.execute(
        "MATCH ()-[r]->() WHERE r.w % 7 = 3 DELETE r",
        &Parameters::new(),
    )
    .map_err(fail)?;
    let expected = records(&db).map_err(|error| error.to_string())?;
    let storage = db.graph.storage();
    let chunks = storage
        .begin_read()
        .and_then(|read| read.scan_prefix(Table::RelationshipChunks, &[]))
        .map_err(|error| error.to_string())?;
    let (pick, edits) = bytes.split_first().unwrap_or((&0, &[]));
    let (key, original) = &chunks[usize::from(*pick) % chunks.len()];
    let damaged = damage(original, edits, &chunks);
    let mut write = storage.begin_write().map_err(|error| error.to_string())?;
    write
        .put(Table::RelationshipChunks, key, &damaged)
        .and_then(|()| write.commit())
        .map_err(|error| error.to_string())?;
    if damaged == *original {
        return Ok(());
    }
    if let Ok(found) = records(&db) {
        return Err(if found == expected {
            "a damaged chunk read without an error".to_owned()
        } else {
            "a damaged chunk read as other relationships".to_owned()
        });
    }
    let read = db.graph.read().map_err(|error| error.to_string())?;
    for id in expected.keys() {
        match read.relationship(*id) {
            Ok(found) if found.as_ref() != expected.get(id) => {
                return Err(format!("relationship {} read as {found:?}", id.0));
            }
            _ => {}
        }
    }
    drop(read);
    for query in [
        "MATCH ()-[r]->() RETURN r ORDER BY id(r)",
        "MATCH (a:A)-[r]->() WHERE a.i < 40 RETURN a.i, r.w, r.s",
        "MATCH ()-[r]->() SET r.w = r.w + 1",
    ] {
        let _ = db.execute(query, &Parameters::new());
    }
    if db.check().is_ok() {
        return Err("the check passed a damaged chunk".to_owned());
    }
    Ok(())
}

const RESEALED_SETUP: [&str; 5] = [
    "UNWIND range(0, 99) AS i INSERT (:A {i: i, s: 'word ' + toString(i % 7) + ' word', v: [toFloat(i % 5), 1.0], l: [i, -i]})-[:R {w: i, s: toString(i)}]->(:B {i: -i})",
    "MATCH (a:A) WHERE a.i % 3 = 0 SET a:C, a.f = toFloat(a.i) / 3.0",
    "MATCH (a:A) WHERE a.i % 11 = 0 REMOVE a:A SET a.s = 'other word'",
    "MATCH ()-[r]->() WHERE r.w % 7 = 3 DELETE r",
    "MATCH (b:B) WHERE b.i % 13 = 0 DETACH DELETE b",
];

const RESEALED_PROBES: [&str; 10] = [
    "MATCH (n) RETURN n, labels(n) ORDER BY id(n)",
    "MATCH (a)-[r]->(b) RETURN id(a), r, id(b) ORDER BY id(r)",
    "MATCH (a)<-[r:R]-(b) WHERE a.i < 0 RETURN count(r)",
    "MATCH (n:A {i: 4}) RETURN n.s",
    "CALL db.index.keyword.queryNodes('words', 'word 3 \"word 3\" wor*', 10) YIELD node, score RETURN node, score",
    "CALL db.index.vector.queryNodes('vectors', [1.0, 2.0], 10) YIELD node, score RETURN node, score",
    "MATCH (n:A) WHERE n.i % 2 = 0 SET n.i = n.i + 1, n:Z, n.s = 'new word'",
    "MATCH ()-[r]->() WHERE r.w % 3 = 0 SET r.w = -r.w",
    "MATCH ()-[r]->() WHERE r.w % 5 = 0 DELETE r",
    "MATCH (n:C) DETACH DELETE n",
];

const RESEALED_TABLES: [Table; 11] = [
    Table::NodeChunks,
    Table::NodeColumns,
    Table::Adjacency,
    Table::RelationshipChunks,
    Table::LabelSets,
    Table::Indexes,
    Table::KeywordPostings,
    Table::KeywordDocuments,
    Table::KeywordTotals,
    Table::Vectors,
    Table::PropertyEntries,
];

/// Damages one stored entry of a graph with every kind of graph entry by `bytes`, writes its
/// checksum again so the damage passes it, and reads, writes, and checks the graph. The first
/// byte picks the table, the second the entry, and each three bytes after them change the entry
/// as [`read_damaged_relationship_chunks`] does. Damage under a valid checksum may read as
/// other data, so only a panic, a hang, or an allocation the fuzzer bounds is a bug.
pub(crate) fn read_resealed_entries(bytes: &[u8]) -> Result<(), String> {
    let db = every_kind_of_entry()?;
    let [table, entry, edits @ ..] = bytes else {
        return Ok(());
    };
    let table = RESEALED_TABLES[usize::from(*table) % RESEALED_TABLES.len()];
    let entries = stored(&db, table)?;
    let Some((key, original)) = entries.get(usize::from(*entry) % entries.len().max(1)) else {
        return Ok(());
    };
    let mut damaged = damage(original, edits, &entries);
    reseal(table, key, &mut damaged);
    put(&db, table, key, &damaged)?;
    for query in RESEALED_PROBES {
        let _ = db.execute(query, &Parameters::new());
    }
    let _ = db.check();
    Ok(())
}

fn every_kind_of_entry() -> Result<Database, String> {
    use super::IndexTarget;
    use super::VectorMetric;

    let fail = |error: crate::Error| error.to_string();
    let db = Database::in_memory().map_err(fail)?;
    db.create_keyword_index("words", IndexTarget::Label("A"), &["s"])
        .map_err(fail)?;
    db.create_vector_index(
        "vectors",
        IndexTarget::Label("A"),
        "v",
        2,
        VectorMetric::Euclidean,
    )
    .map_err(fail)?;
    db.create_property_index("by_i", IndexTarget::Label("A"), "i")
        .map_err(fail)?;
    for query in RESEALED_SETUP {
        db.execute(query, &Parameters::new()).map_err(fail)?;
    }
    Ok(db)
}

fn stored(db: &Database, table: Table) -> Result<Vec<Entry>, String> {
    db.graph
        .storage()
        .begin_read()
        .and_then(|read| read.scan_prefix(table, &[]))
        .map_err(|error| error.to_string())
}

fn put(db: &Database, table: Table, key: &[u8], value: &[u8]) -> Result<(), String> {
    let mut write = db
        .graph
        .storage()
        .begin_write()
        .map_err(|error| error.to_string())?;
    write
        .put(table, key, value)
        .and_then(|()| write.commit())
        .map_err(|error| error.to_string())
}

fn reseal(table: Table, key: &[u8], bytes: &mut [u8]) {
    match table {
        Table::NodeChunks => super::meta::reseal_for_fuzzing(key, bytes),
        Table::NodeColumns => super::columns::reseal_for_fuzzing(key, bytes),
        Table::Adjacency => super::adjacency::reseal_for_fuzzing(key, bytes),
        Table::RelationshipChunks => super::relationships::reseal_for_fuzzing(key, bytes),
        _ => {}
    }
}

fn records(
    db: &Database,
) -> Result<BTreeMap<RelationshipId, RelationshipRecord>, super::GraphError> {
    let mut found = BTreeMap::new();
    db.graph.read()?.visit_relationship_records(|id, bytes| {
        found.insert(id, RelationshipRecord::decode(bytes)?);
        Ok(())
    })?;
    Ok(found)
}

#[cfg(test)]
#[path = "fuzz.test.rs"]
mod tests;
