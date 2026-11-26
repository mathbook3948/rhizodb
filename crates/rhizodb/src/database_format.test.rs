use super::*;
use crate::engine::codec::checksum;
use crate::engine::header::FORMAT_MINOR;
use crate::engine::layout::BLOCK;

fn with_engine_minor(path: &Path, minor: u16) {
    let mut bytes = std::fs::read(path).unwrap();
    for slot in 0..2 {
        let block = &mut bytes[slot * BLOCK..(slot + 1) * BLOCK];
        block[10..12].copy_from_slice(&minor.to_le_bytes());
        let sum = checksum(0, &[&block[..BLOCK - 8]]);
        block[BLOCK - 8..].copy_from_slice(&sum.to_le_bytes());
    }
    std::fs::write(path, bytes).unwrap();
}

#[test]
fn a_file_of_a_newer_engine_format_fails_to_open_as_an_unsupported_format_version() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("newer.rhizodb");
    {
        let db = Database::open(&path).unwrap();
        db.execute("INSERT (:Item {n: 1})", &Parameters::new())
            .unwrap();
    }
    with_engine_minor(&path, FORMAT_MINOR + 1);
    let error = Database::open(&path).err().unwrap();
    assert_eq!(error.kind(), ErrorKind::UnsupportedFormatVersion, "{error}");
}

#[test]
fn a_file_of_the_current_engine_format_still_opens_after_resealing() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("current.rhizodb");
    {
        let db = Database::open(&path).unwrap();
        db.execute("INSERT (:Item {n: 1})", &Parameters::new())
            .unwrap();
    }
    with_engine_minor(&path, FORMAT_MINOR);
    let db = Database::open(&path).unwrap();
    let rows = db
        .execute("MATCH (i:Item) RETURN i.n", &Parameters::new())
        .unwrap();
    assert_eq!(rows.rows().len(), 1);
}
