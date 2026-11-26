use std::collections::BTreeSet;

use proptest::prelude::*;

use super::*;

#[test]
fn every_graph_table_has_its_own_id_in_the_graph_range() {
    let ids: BTreeSet<TableId> = Table::ALL.into_iter().map(TableId::of).collect();
    assert_eq!(ids.len(), Table::ALL.len());
    for table in Table::ALL {
        let id = TableId::of(table);
        assert!((0x100..0x200).contains(&id.0), "{table:?} has {id:?}");
        assert_eq!(id.table().unwrap(), table);
    }
}

#[test]
fn every_table_is_listed_once_in_declaration_order() {
    for (at, table) in Table::ALL.into_iter().enumerate() {
        assert_eq!(table as usize, at, "{table:?}");
    }
    assert_eq!(
        Table::ALL.last().map(|table| *table as usize + 1),
        Some(Table::ALL.len())
    );
}

#[test]
fn the_table_ids_never_change() {
    let ids: Vec<u16> = Table::ALL
        .into_iter()
        .map(|table| TableId::of(table).0)
        .collect();
    assert_eq!(ids, (0x100..0x114).collect::<Vec<u16>>());
}

#[test]
fn an_unknown_table_id_is_unsupported() {
    for raw in [0, 1, 2, 0xFF, 0x114, 0x1FF, 0xFFFF] {
        assert!(matches!(
            TableId(raw).table(),
            Err(EngineError::Unsupported(_))
        ));
    }
}

#[test]
fn a_block_ref_splits_into_extent_and_block() {
    let reference = BlockRef::new(ExtentId(3), 17).unwrap();
    assert_eq!(reference.raw(), (3 << 20) | 17);
    assert_eq!(reference.extent(), ExtentId(3));
    assert_eq!(reference.block(), 17);
    assert_eq!(reference.offset(5).unwrap().block(), 22);
    assert!(reference.offset(EXTENT_BLOCKS - 17).is_err());
    assert_eq!(format!("{reference:?}"), "BlockRef(3:17)");
}

#[test]
fn a_block_ref_outside_any_extent_is_refused() {
    assert!(BlockRef::new(ExtentId::NONE, 0).is_err());
    assert!(BlockRef::new(ExtentId(1), EXTENT_BLOCKS).is_err());
    assert!(BlockRef::new(ExtentId(1 << 44), 0).is_err());
    assert!(BlockRef::from_raw(0).unwrap().is_null());
    assert!(BlockRef::from_raw(5).is_err());
    assert!(BlockRef::from_raw((1 << 20) | u64::from(EXTENT_BLOCKS)).is_err());
    assert!(BlockRef::from_raw(u64::MAX).is_err());
}

#[test]
fn extents_lie_after_the_header_slots() {
    assert_eq!(physical_block(0, 0).unwrap(), 2);
    assert_eq!(physical_block(0, 1023).unwrap(), 1025);
    assert_eq!(physical_block(1, 0).unwrap(), 1026);
    assert!(physical_block(u64::MAX, 0).is_err());
}

proptest! {
    #[test]
    fn a_block_ref_round_trips(extent in 1u64..(1 << 44), block in 0u32..EXTENT_BLOCKS) {
        let reference = BlockRef::new(ExtentId(extent), block).unwrap();
        prop_assert_eq!(BlockRef::from_raw(reference.raw()).unwrap(), reference);
        prop_assert_eq!(reference.extent(), ExtentId(extent));
        prop_assert_eq!(reference.block(), block);
    }

    #[test]
    fn any_raw_block_ref_decodes_or_fails_without_panicking(raw in any::<u64>()) {
        if let Ok(reference) = BlockRef::from_raw(raw) {
            prop_assert_eq!(reference.raw(), raw);
        }
    }
}
