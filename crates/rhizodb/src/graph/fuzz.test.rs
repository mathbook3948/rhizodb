use proptest::prelude::*;

use super::*;

#[test]
fn a_resealed_entry_of_every_checksummed_table_passes_its_checksum() {
    let tables = [
        Table::NodeChunks,
        Table::NodeColumns,
        Table::Adjacency,
        Table::RelationshipChunks,
    ];
    for table in tables {
        for resealed in [false, true] {
            let db = every_kind_of_entry().unwrap();
            let entries = stored(&db, table).unwrap();
            for (key, value) in &entries {
                let mut damaged = value.clone();
                let last = damaged.len() - 1;
                damaged[last] ^= 0x40;
                if resealed {
                    reseal(table, key, &mut damaged);
                }
                put(&db, table, key, &damaged).unwrap();
            }
            let named = RESEALED_PROBES[..6].iter().any(|query| {
                db.execute(query, &Parameters::new())
                    .is_err_and(|error| error.to_string().contains("checksum"))
            });
            assert_eq!(named, !resealed, "{table:?} resealed {resealed}");
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(crate::test_scale::cases(32)))]

    #[test]
    fn any_damage_of_a_relationship_chunk_fails_its_reads_and_the_check(
        bytes in prop::collection::vec(any::<u8>(), 0..24),
    ) {
        read_damaged_relationship_chunks(&bytes).map_err(TestCaseError::fail)?;
    }

    #[test]
    fn any_damage_of_a_graph_entry_under_a_valid_checksum_reads_without_a_panic(
        bytes in prop::collection::vec(any::<u8>(), 0..24),
    ) {
        read_resealed_entries(&bytes).map_err(TestCaseError::fail)?;
    }
}
