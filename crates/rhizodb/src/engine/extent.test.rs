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
    assert_eq!(table.entries[&ExtentId(1)].live(), EXTENT_BLOCKS);
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
                used: Blocks::range(0, 1000),
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
    assert_eq!(table.entries[&ExtentId(1)].live(), 1);
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

#[test]
fn a_block_set_lists_its_runs_and_finds_the_first_long_enough() {
    let set = Blocks::range(0, 3)
        .with(&Blocks::range(10, 5))
        .with(&Blocks::range(63, 2))
        .with(&Blocks::range(EXTENT_BLOCKS - 1, 4));
    assert_eq!(
        set.ranges().collect::<Vec<_>>(),
        [(0, 3), (10, 5), (63, 2), (EXTENT_BLOCKS - 1, 1)]
    );
    assert_eq!(set.count(), 11);
    assert_eq!(set.first_run(1), Some(0));
    assert_eq!(set.first_run(4), Some(10));
    assert_eq!(set.first_run(6), None);
    assert_eq!(set.without(&Blocks::range(0, 64)).first_run(1), Some(64));
    assert!(Blocks::range(0, EXTENT_BLOCKS).contains_all(&set));
    assert!(!set.contains_all(&Blocks::range(2, 2)));
    assert!(set.intersects(&Blocks::range(14, 40)));
    assert!(!set.intersects(&Blocks::range(15, 40)));
    assert!(Blocks::default().is_empty());
    assert_eq!(
        Blocks::range(0, EXTENT_BLOCKS).ranges().collect::<Vec<_>>(),
        [(0, EXTENT_BLOCKS)]
    );
}

#[test]
fn placing_fills_the_lowest_hole_that_fits_before_the_open_extent_and_appending_skips_holes() {
    let mut table = ExtentTable::default();
    table.place(EXTENT_BLOCKS, |_, _| {}).unwrap();
    table.place(10, |_, _| {}).unwrap();
    let first = BlockRef::new(ExtentId(1), 0).unwrap();
    table.free(first.offset(5).unwrap(), 1).unwrap();
    table.free(first.offset(20).unwrap(), 4).unwrap();
    table
        .free(BlockRef::new(ExtentId(2), 2).unwrap(), 2)
        .unwrap();
    table.holes = vec![
        (ExtentId(1), table.entries[&ExtentId(1)].free()),
        (ExtentId(2), table.entries[&ExtentId(2)].free()),
    ];
    let catalog = table.append(1, |_, _| {}).unwrap();
    assert_eq!(catalog, BlockRef::new(ExtentId(2), 10).unwrap());
    let run = table.place(3, |_, _| {}).unwrap();
    assert_eq!(run, first.offset(20).unwrap());
    let page = table.place(1, |_, _| {}).unwrap();
    assert_eq!(page, first.offset(5).unwrap());
    let next = table.place(2, |_, _| {}).unwrap();
    assert_eq!(next, BlockRef::new(ExtentId(2), 2).unwrap());
    let last = table.place(1, |_, _| {}).unwrap();
    assert_eq!(last, first.offset(23).unwrap());
    let appended = table.place(1, |_, _| {}).unwrap();
    assert_eq!(appended, BlockRef::new(ExtentId(2), 11).unwrap());
    assert_eq!(table.entries[&ExtentId(1)].live(), EXTENT_BLOCKS);
    assert_eq!(table.entries[&ExtentId(2)].live(), 12);
    assert!(table.holes.iter().all(|(_, free)| free.is_empty()));
}

#[test]
fn a_hole_that_a_page_already_holds_is_refused() {
    let mut table = ExtentTable::default();
    table.place(4, |_, _| {}).unwrap();
    table.holes = vec![(ExtentId(1), Blocks::range(1, 1))];
    assert!(table.place(1, |_, _| {}).unwrap_err().is_corrupt());
}

#[test]
fn fitting_the_file_drops_free_slots_past_the_last_extent_and_refuses_an_extent_past_its_end() {
    let mut table = with_an_open_extent_per_purpose();
    table.next_slot = 1 << 40;
    let file_blocks = physical_block(3, 0).unwrap();

    table.fit_file(file_blocks).unwrap();

    assert_eq!(table.next_slot, 3);
    assert!(table.free_slots().is_empty());
    let error = table.fit_file(physical_block(2, 0).unwrap()).unwrap_err();
    assert!(error.is_corrupt(), "{error}");
}
