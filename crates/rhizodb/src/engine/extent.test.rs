use super::*;

#[test]
fn placing_fills_one_extent_with_every_kind_of_block_then_seals_it_and_opens_the_next() {
    let mut table = ExtentTable::default();
    let mut opened = Vec::new();
    let first = table.place(1, |id, slot| opened.push((id, slot))).unwrap();
    assert_eq!(first, BlockRef::new(ExtentId(1), 0).unwrap());
    let run = table.place(3, |id, slot| opened.push((id, slot))).unwrap();
    assert_eq!(run, BlockRef::new(ExtentId(1), 1).unwrap());
    let rest = table
        .place(EXTENT_BLOCKS - 4, |id, slot| opened.push((id, slot)))
        .unwrap();
    assert_eq!(rest, BlockRef::new(ExtentId(1), 4).unwrap());
    let next = table.place(1, |id, slot| opened.push((id, slot))).unwrap();
    assert_eq!(next, BlockRef::new(ExtentId(2), 0).unwrap());
    assert_eq!(opened, [(ExtentId(1), 0), (ExtentId(2), 1)]);
    assert_eq!(table.entries[&ExtentId(1)].state, ExtentState::Sealed);
    assert_eq!(table.entries[&ExtentId(1)].live, EXTENT_BLOCKS);
    assert_eq!(table.open, [Some(ExtentId(2)), None, None, None]);
    table.check_open(&table.open_extents().unwrap()).unwrap();
}

fn with_an_open_extent_per_purpose() -> ExtentTable {
    let mut table = ExtentTable::default();
    for (index, purpose) in Purpose::ALL.into_iter().enumerate() {
        let id = ExtentId(index as u64 + 1);
        table.entries.insert(
            id,
            ExtentEntry {
                slot: index as u64,
                written: 1000,
                live: 1000,
                state: ExtentState::Open,
                purpose,
                scheme: 0,
                opened_term: 0,
            },
        );
        table.open[index] = Some(id);
    }
    table.next_extent = ExtentId(4);
    table.next_slot = 3;
    table.check_open(&table.open_extents().unwrap()).unwrap();
    table
}

#[test]
fn an_older_file_fills_the_room_left_in_each_open_extent_before_it_opens_one() {
    let mut table = with_an_open_extent_per_purpose();
    let mut opened = Vec::new();
    let tree = table.place(20, |id, slot| opened.push((id, slot))).unwrap();
    let values = table.place(20, |id, slot| opened.push((id, slot))).unwrap();
    let meta = table.place(24, |id, slot| opened.push((id, slot))).unwrap();
    assert_eq!(
        [tree, values, meta],
        [
            BlockRef::new(ExtentId(1), 1000).unwrap(),
            BlockRef::new(ExtentId(2), 1000).unwrap(),
            BlockRef::new(ExtentId(3), 1000).unwrap(),
        ]
    );
    assert!(opened.is_empty());
    let next = table.place(5, |id, slot| opened.push((id, slot))).unwrap();
    assert_eq!(next, BlockRef::new(ExtentId(4), 0).unwrap());
    assert_eq!(opened, [(ExtentId(4), 3)]);
    assert_eq!(table.open, [Some(ExtentId(4)), None, None, None]);
    for id in 1..=3 {
        assert_eq!(table.entries[&ExtentId(id)].state, ExtentState::Sealed);
    }
    table.check_open(&table.open_extents().unwrap()).unwrap();
}

#[test]
fn freeing_counts_down_and_refuses_what_is_not_live() {
    let mut table = ExtentTable::default();
    let at = table.place(4, |_, _| {}).unwrap();
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
    assert!(table.place(0, |_, _| {}).is_err());
    assert!(table.place(EXTENT_BLOCKS + 1, |_, _| {}).is_err());
}

#[test]
fn open_extents_that_disagree_with_the_header_are_corrupt() {
    let mut table = ExtentTable::default();
    table.place(2, |_, _| {}).unwrap();
    let mut header = table.open_extents().unwrap();
    table.check_open(&header).unwrap();
    header[0].written = 3;
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
