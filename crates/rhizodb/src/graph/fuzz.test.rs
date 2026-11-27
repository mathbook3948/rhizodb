use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(crate::test_scale::cases(32)))]

    #[test]
    fn any_damage_of_a_relationship_chunk_fails_its_reads_and_the_check(
        bytes in prop::collection::vec(any::<u8>(), 0..24),
    ) {
        super::read_damaged_relationship_chunks(&bytes).map_err(TestCaseError::fail)?;
    }
}
