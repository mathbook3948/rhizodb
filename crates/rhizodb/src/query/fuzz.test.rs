use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(crate::test_scale::cases(32)))]

    #[test]
    fn any_parameters_run_every_query_without_a_panic(
        bytes in prop::collection::vec(any::<u8>(), 0..64),
    ) {
        super::run_with_parameters(&bytes).map_err(TestCaseError::fail)?;
    }
}
