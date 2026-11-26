use proptest::prelude::*;

use super::Position;
use crate::error::ErrorKind;

proptest! {
    #[test]
    fn a_position_reads_back_from_its_bytes(
        term in 1u64..,
        writer_id in proptest::collection::vec(any::<u8>(), 1..=64),
        lsn in any::<u64>(),
    ) {
        let position = Position::new(term, &writer_id, lsn).map_err(|error| TestCaseError::fail(error.to_string()))?;
        let read = Position::from_bytes(&position.to_bytes()).map_err(|error| TestCaseError::fail(error.to_string()))?;
        prop_assert_eq!(read, position);
    }

    #[test]
    fn arbitrary_bytes_fail_as_corrupt_data_or_read_back_the_same(bytes in proptest::collection::vec(any::<u8>(), 0..100)) {
        match Position::from_bytes(&bytes) {
            Ok(position) => prop_assert_eq!(position.to_bytes(), bytes),
            Err(error) => prop_assert_eq!(error.kind(), ErrorKind::CorruptData),
        }
    }
}

#[test]
fn only_term_zero_has_no_writer_id() {
    assert!(Position::new(0, b"", 7).is_ok());
    for (term, writer_id) in [(0, &b"a"[..]), (1, b""), (1, &[0; 65])] {
        let error = Position::new(term, writer_id, 1).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidArgumentValue);
    }
}
