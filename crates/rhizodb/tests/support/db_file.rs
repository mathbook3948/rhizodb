//! A database file that holds every kind of record and index, damage that a few bytes
//! describe, and the reads and writes a damaged file must survive without a panic. Shared by
//! the `corrupt_file` integration test and the `db_file` fuzz target.

use std::path::Path;

use rhizodb::Database;
use rhizodb::IndexTarget;
use rhizodb::Parameters;
use rhizodb::VectorMetric;

const PAGE: usize = 4096;
const MAX_DAMAGES: usize = 4;

/// How many bytes describe one damage.
pub const DAMAGE_BYTES: usize = 6;

const SETUP: &[&str] = &[
    "INSERT (:Person {name: 'Ada', age: 36, tags: ['a', 'b'], score: 1.5, \
     born: date('1815-12-10')})-[:KNOWS {since: 1833}]->(:Person {name: 'Charles', age: 79})",
    "UNWIND range(1, 300) AS i INSERT (:Item {i: i, text: 'word ' + toString(i), \
     vec: [toFloat(i), 1.0], blob: 'x' + toString(i * 7919)})",
    "MATCH (a:Item), (b:Item) WHERE b.i = a.i + 1 INSERT (a)-[:NEXT {w: a.i}]->(b)",
    "MATCH (n:Item) WHERE n.i % 7 = 0 DETACH DELETE n",
    "MATCH (n:Item) WHERE n.i % 5 = 0 SET n:Five REMOVE n.blob",
];

const PROBES: &[&str] = &[
    "MATCH (n) RETURN count(n)",
    "MATCH (n) RETURN n ORDER BY id(n)",
    "MATCH ()-[r]->() RETURN r ORDER BY id(r)",
    "MATCH (n:Item {i: 42}) RETURN n.text",
    "MATCH (n:Item) WHERE n.i > 100 RETURN count(n), sum(n.i)",
    "MATCH (n:Five) RETURN labels(n), keys(n)",
    "MATCH (:Person {name: 'Ada'})-[k:KNOWS {since: 1833}]->(b) RETURN b, k",
    "MATCH p = (:Item)-[:NEXT*1..3]->(:Item) RETURN count(p)",
    "CALL db.index.keyword.queryNodes('text', 'word', 10) YIELD node, score RETURN node, score",
    "CALL db.index.vector.queryNodes('vec', [3.0, 1.0], 10) YIELD node, score RETURN node, score",
    "MATCH (n:Item) WHERE n.i < 20 SET n.i = n.i + 1000 INSERT (:Fresh {n: 1})",
    "MATCH (n:Fresh) RETURN count(n)",
];

/// Writes the undamaged database to `path` and returns its bytes.
pub fn pristine(path: &Path) -> Vec<u8> {
    let db = Database::open(path).unwrap();
    db.create_property_index("item_i", IndexTarget::Label("Item"), "i")
        .unwrap();
    db.create_keyword_index("text", IndexTarget::Label("Item"), &["text"])
        .unwrap();
    db.create_vector_index(
        "vec",
        IndexTarget::Label("Item"),
        "vec",
        2,
        VectorMetric::Cosine,
    )
    .unwrap();
    db.create_property_index(
        "knows_since",
        IndexTarget::RelationshipType("KNOWS"),
        "since",
    )
    .unwrap();
    for query in SETUP {
        db.execute(query, &Parameters::new()).unwrap();
    }
    drop(db);
    std::fs::read(path).unwrap()
}

/// Applies up to `MAX_DAMAGES` damages, `DAMAGE_BYTES` bytes each: a kind, a value, and a
/// little-endian offset into the file.
pub fn damage(mut file: Vec<u8>, bytes: &[u8]) -> Vec<u8> {
    for chunk in bytes.as_chunks::<DAMAGE_BYTES>().0.iter().take(MAX_DAMAGES) {
        if file.is_empty() {
            break;
        }
        let offset = u32::from_le_bytes([chunk[2], chunk[3], chunk[4], chunk[5]]) as usize;
        let at = offset % file.len();
        let value = chunk[1];
        match chunk[0] % 6 {
            0 => file[at] = value,
            1 => file[at] ^= 1 << (value % 8),
            2 => file.truncate(at),
            3 => {
                let start = at - at % PAGE;
                let end = (start + PAGE).min(file.len());
                file[start..end].fill(value);
            }
            4 => {
                let source = (usize::from(value) * PAGE) % file.len();
                let source = source - source % PAGE;
                let target = at - at % PAGE;
                let length = PAGE.min(file.len() - source).min(file.len() - target);
                file.copy_within(source..source + length, target);
            }
            _ => {
                let end = (at + 8).min(file.len());
                file[at..end].fill(0xff);
            }
        }
    }
    file
}

/// Writes the damaged `file` to `path`, opens it, and runs every probe. Any outcome but a
/// panic or a hang is acceptable, since the damage may make the file unreadable.
pub fn open_and_probe(path: &Path, file: &[u8]) {
    std::fs::write(path, file).unwrap();
    let Ok(db) = Database::open(path) else {
        return;
    };
    for query in PROBES {
        let _ = db.execute(query, &Parameters::new());
    }
}
