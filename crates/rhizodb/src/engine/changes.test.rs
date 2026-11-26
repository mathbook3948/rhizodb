use super::EpochId;
use super::decode;
use super::encode;

#[test]
fn empty_changes_read_back() {
    let after = EpochId {
        term: 3,
        writer_id: b"w".to_vec(),
    };
    let bytes = encode(7, &after, 9, 42, &[]).unwrap();
    let decoded = decode(&bytes).unwrap();
    assert_eq!(
        (decoded.after, decoded.after_lsn, decoded.last_chain),
        (after, 9, 42)
    );
    assert!(decoded.records.is_empty());
}
