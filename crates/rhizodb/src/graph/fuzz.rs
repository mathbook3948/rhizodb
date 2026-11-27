use std::collections::BTreeMap;

use crate::Database;
use crate::result::Parameters;
use crate::storage::ReadTransaction;
use crate::storage::Storage;
use crate::storage::Table;
use crate::storage::WriteTransaction;

use super::RelationshipId;
use super::RelationshipRecord;

const SETUP: &str = "UNWIND range(0, 79) AS i INSERT (:A {i: i})-[:R {w: i, s: 'text ' + toString(i), l: [i, -i]}]->(:B)";

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
    let mut damaged = original.clone();
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
            _ => damaged.clone_from(&chunks[usize::from(*value) % chunks.len()].1),
        }
    }
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
