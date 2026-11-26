use super::EpochId;
use super::decode;
use super::encode;

#[test]
fn empty_changes_read_back() {
    let after = EpochId {
        term: 3,
        writer_id: b"w".to_vec(),
    };
    let bytes = encode(&after, 9, &[]).unwrap();
    let decoded = decode(&bytes).unwrap();
    assert_eq!((decoded.after, decoded.after_lsn), (after, 9));
    assert!(decoded.records.is_empty());
}
