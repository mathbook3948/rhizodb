use super::*;

#[test]
fn placing_fills_an_extent_then_seals_it_and_opens_the_next() {
    let mut table = ExtentTable::default();
    let mut opened = Vec::new();
    let first = table
        .place(Purpose::Tree, 1, |id, slot| opened.push((id, slot)))
        .unwrap();
    assert_eq!(first, BlockRef::new(ExtentId(1), 0).unwrap());
    let values = table
        .place(Purpose::Values, 3, |id, slot| opened.push((id, slot)))
        .unwrap();
    assert_eq!(values.extent(), ExtentId(2));
    let rest = table
        .place(Purpose::Tree, EXTENT_BLOCKS - 1, |id, slot| {
            opened.push((id, slot))
        })
        .unwrap();
    assert_eq!(rest, BlockRef::new(ExtentId(1), 1).unwrap());
    let next = table
        .place(Purpose::Tree, 1, |id, slot| opened.push((id, slot)))
        .unwrap();
    assert_eq!(next, BlockRef::new(ExtentId(3), 0).unwrap());
    assert_eq!(
        opened,
        [(ExtentId(1), 0), (ExtentId(2), 1), (ExtentId(3), 2)]
    );
    assert_eq!(table.entries[&ExtentId(1)].state, ExtentState::Sealed);
    assert_eq!(table.entries[&ExtentId(1)].live, EXTENT_BLOCKS);
    assert_eq!(table.open[0], Some(ExtentId(3)));
    table.check_open(&table.open_extents().unwrap()).unwrap();
}

#[test]
fn freeing_counts_down_and_refuses_what_is_not_live() {
    let mut table = ExtentTable::default();
    let at = table.place(Purpose::Values, 4, |_, _| {}).unwrap();
    table.free(at, 3).unwrap();
    assert_eq!(table.entries[&ExtentId(1)].live, 1);
    assert!(table.free(at, 2).unwrap_err().is_corrupt());
    assert!(
        table
            .free(BlockRef::new(ExtentId(9), 0).unwrap(), 1)
            .is_err()
    );
    assert!(table.free(at.offset(4).unwrap(), 1).is_err());
}

#[test]
fn a_run_larger_than_an_extent_or_empty_is_refused() {
    let mut table = ExtentTable::default();
    assert!(table.place(Purpose::Tree, 0, |_, _| {}).is_err());
    assert!(
        table
            .place(Purpose::Tree, EXTENT_BLOCKS + 1, |_, _| {})
            .is_err()
    );
}

#[test]
fn open_extents_that_disagree_with_the_header_are_corrupt() {
    let mut table = ExtentTable::default();
    table.place(Purpose::Meta, 2, |_, _| {}).unwrap();
    let mut header = table.open_extents().unwrap();
    table.check_open(&header).unwrap();
    header[2].written = 3;
    assert!(table.check_open(&header).unwrap_err().is_corrupt());
    let mut twin = table.clone();
    let entry = *twin.entries.values().next().unwrap();
    twin.entries.insert(
        ExtentId(2),
        ExtentEntry {
            state: ExtentState::Sealed,
            ..entry
        },
    );
    twin.next_extent = ExtentId(3);
    twin.next_slot = 1;
    assert!(twin.check_open(&twin.open_extents().unwrap()).is_err());
}
