use std::collections::BTreeSet;

use super::*;

const SEEDS: u64 = 400;

const BLOCK: usize = 4096;

type Block = [u8; BLOCK];

fn size() -> BlockSize {
    BlockSize::new(BLOCK).unwrap()
}

fn page(fill: u8) -> Block {
    [fill; BLOCK]
}

fn pages(fs: &SimFs, name: &str) -> Vec<Block> {
    let file = fs.open(name, size()).unwrap();
    let mut pages = vec![page(0); file.block_count().unwrap() as usize];
    file.read(0, pages.as_flattened_mut()).unwrap();
    pages
}

fn synced_file(fs: &SimFs, name: &str, contents: &[Block]) -> SimFile {
    let file = fs.create(name, size()).unwrap();
    file.write(0, contents.as_flattened()).unwrap();
    file.sync().unwrap();
    fs.sync_names().unwrap();
    file
}

#[test]
fn reads_see_writes_that_are_not_synced_yet() {
    let fs = SimFs::new();
    let file = fs.create("db", size()).unwrap();
    file.write(0, [page(1), page(2)].as_flattened()).unwrap();
    assert_eq!(pages(&fs, "db"), [page(1), page(2)]);
}

#[test]
fn a_power_loss_keeps_synced_writes_and_drops_the_rest() {
    let fs = SimFs::new();
    let file = synced_file(&fs, "db", &[page(1), page(2)]);
    file.write(1, [page(3)].as_flattened()).unwrap();
    file.write(2, [page(4)].as_flattened()).unwrap();

    let crashed = fs.crash(Crash::DropUnsynced);
    assert_eq!(pages(&crashed, "db"), [page(1), page(2)]);
    let kept = fs.crash(Crash::KeepUnsynced);
    assert_eq!(pages(&kept, "db"), [page(1), page(3), page(4)]);
    assert_eq!(pages(&fs, "db"), [page(1), page(3), page(4)]);
}

#[test]
fn a_sync_covers_only_its_own_file() {
    let fs = SimFs::new();
    let first = synced_file(&fs, "first", &[page(1)]);
    let second = synced_file(&fs, "second", &[page(2)]);
    first.write(0, [page(3)].as_flattened()).unwrap();
    second.write(0, [page(4)].as_flattened()).unwrap();
    first.sync().unwrap();

    let crashed = fs.crash(Crash::DropUnsynced);
    assert_eq!(pages(&crashed, "first"), [page(3)]);
    assert_eq!(pages(&crashed, "second"), [page(2)]);
}

#[test]
fn a_block_count_change_needs_a_sync() {
    let fs = SimFs::new();
    let file = synced_file(&fs, "db", &[page(1), page(2)]);
    file.set_block_count(1).unwrap();
    assert_eq!(
        pages(&fs.crash(Crash::DropUnsynced), "db"),
        [page(1), page(2)]
    );
    file.set_block_count(4).unwrap();
    file.write(5, [page(6)].as_flattened()).unwrap();
    assert_eq!(
        pages(&fs.crash(Crash::DropUnsynced), "db"),
        [page(1), page(2)]
    );
    file.sync().unwrap();
    assert_eq!(
        pages(&fs.crash(Crash::DropUnsynced), "db"),
        [page(1), page(0), page(0), page(0), page(0), page(6)]
    );
}

#[test]
fn a_synced_file_whose_name_was_never_synced_is_lost() {
    let fs = SimFs::new();
    let file = fs.create("db", size()).unwrap();
    file.write(0, [page(1)].as_flattened()).unwrap();
    file.sync().unwrap();
    assert!(fs.crash(Crash::DropUnsynced).names().unwrap().is_empty());
    fs.sync_names().unwrap();
    assert_eq!(pages(&fs.crash(Crash::DropUnsynced), "db"), [page(1)]);
}

#[test]
fn a_rename_or_remove_needs_a_sync_of_the_names() {
    let fs = SimFs::new();
    synced_file(&fs, "old", &[page(1)]);
    synced_file(&fs, "target", &[page(2)]);
    fs.rename("old", "target").unwrap();
    let crashed = fs.crash(Crash::DropUnsynced);
    assert_eq!(crashed.names().unwrap(), ["old", "target"]);
    assert_eq!(pages(&crashed, "target"), [page(2)]);
    assert_eq!(pages(&fs.crash(Crash::KeepUnsynced), "target"), [page(1)]);

    fs.sync_names().unwrap();
    fs.remove("target").unwrap();
    let crashed = fs.crash(Crash::DropUnsynced);
    assert_eq!(crashed.names().unwrap(), ["target"]);
    assert_eq!(pages(&crashed, "target"), [page(1)]);
    assert!(fs.crash(Crash::KeepUnsynced).names().unwrap().is_empty());
}

#[test]
fn a_file_keeps_working_after_its_name_is_removed() {
    let fs = SimFs::new();
    let file = synced_file(&fs, "db", &[page(1)]);
    fs.remove("db").unwrap();
    file.write(0, [page(2)].as_flattened()).unwrap();
    let mut read = [page(0)];
    file.read(0, read.as_flattened_mut()).unwrap();
    assert_eq!(read, [page(2)]);
    assert!(fs.names().unwrap().is_empty());
}

#[test]
fn two_handles_of_one_file_share_its_pages() {
    let fs = SimFs::new();
    let writer = fs.create("db", size()).unwrap();
    let reader = fs.open("db", size()).unwrap();
    writer.write(0, [page(5)].as_flattened()).unwrap();
    let mut read = [page(0)];
    reader.read(0, read.as_flattened_mut()).unwrap();
    assert_eq!(read, [page(5)]);
    reader.sync().unwrap();
    fs.sync_names().unwrap();
    assert_eq!(pages(&fs.crash(Crash::DropUnsynced), "db"), [page(5)]);
}

#[test]
fn a_crashed_file_system_starts_fully_synced_and_keeps_working() {
    let fs = SimFs::new();
    let file = synced_file(&fs, "db", &[page(1)]);
    file.write(1, [page(2)].as_flattened()).unwrap();
    fs.fail_after(0, Failure::FromThenOn);
    let crashed = fs.crash(Crash::KeepUnsynced);
    assert_eq!(
        pages(&crashed.crash(Crash::DropUnsynced), "db"),
        [page(1), page(2)]
    );
    let file = crashed.open("db", size()).unwrap();
    file.write(0, [page(3)].as_flattened()).unwrap();
    assert_eq!(pages(&crashed, "db"), [page(3), page(2)]);
    assert!(fs.names().is_err());
}

fn sectors(page: &Block) -> &[[u8; SECTOR_SIZE]] {
    page.as_chunks::<SECTOR_SIZE>().0
}

#[test]
fn a_scrambled_power_loss_never_touches_synced_data_and_reaches_every_outcome() {
    let mut whole_outcomes = BTreeSet::new();
    let mut sector_outcomes = BTreeSet::new();
    let mut lengths = BTreeSet::new();
    let mut name_outcomes = BTreeSet::new();
    for seed in 0..SEEDS {
        let fs = SimFs::new();
        let file = synced_file(&fs, "db", &[page(1), page(2)]);
        synced_file(&fs, "other", &[page(9)]);
        file.write(1, [page(3)].as_flattened()).unwrap();
        file.write(1, [page(4)].as_flattened()).unwrap();
        file.write(3, [page(5)].as_flattened()).unwrap();
        fs.create("new", size()).unwrap();

        let crashed = fs.crash(Crash::Scramble { seed });
        assert_eq!(pages(&crashed, "other"), [page(9)]);
        let image = pages(&crashed, "db");
        assert_eq!(image[0], page(1));
        lengths.insert(image.len());
        whole_outcomes.insert(image[1][0]);
        for sector in sectors(&image[1]) {
            let fill = sector[0];
            if sector.iter().all(|byte| *byte == fill) && [2, 3, 4].contains(&fill) {
                sector_outcomes.insert(Some(fill));
            } else {
                sector_outcomes.insert(None);
            }
        }
        if image.len() == 4 {
            assert_eq!(image[2], page(0));
        }
        name_outcomes.insert(crashed.names().unwrap().len());
    }
    assert!(whole_outcomes.is_superset(&BTreeSet::from([2, 3, 4])));
    assert_eq!(
        sector_outcomes,
        BTreeSet::from([None, Some(2), Some(3), Some(4)])
    );
    assert_eq!(lengths, BTreeSet::from([2, 4]));
    assert_eq!(name_outcomes, BTreeSet::from([2, 3]));
}

#[test]
fn a_reordered_power_loss_keeps_each_sector_old_or_new_and_reaches_both() {
    let mut sector_outcomes = BTreeSet::new();
    for seed in 0..SEEDS {
        let fs = SimFs::new();
        let file = synced_file(&fs, "db", &[page(1), page(2)]);
        file.write(1, [page(3)].as_flattened()).unwrap();
        file.write(1, [page(4)].as_flattened()).unwrap();
        let image = pages(&fs.crash(Crash::Reorder { seed }), "db");
        assert_eq!(image[0], page(1));
        for sector in sectors(&image[1]) {
            let fill = sector[0];
            assert!(
                sector.iter().all(|byte| *byte == fill),
                "a sector is torn inside"
            );
            sector_outcomes.insert(fill);
        }
    }
    assert_eq!(sector_outcomes, BTreeSet::from([2, 3, 4]));
}

#[test]
fn a_scrambled_power_loss_can_lose_a_block_count_change() {
    let mut lengths = BTreeSet::new();
    for seed in 0..SEEDS {
        let fs = SimFs::new();
        let file = synced_file(&fs, "db", &[page(1), page(2), page(3)]);
        file.set_block_count(1).unwrap();
        let image = pages(&fs.crash(Crash::Scramble { seed }), "db");
        assert!(image == [page(1)] || image == [page(1), page(2), page(3)]);
        lengths.insert(image.len());
    }
    assert_eq!(lengths, BTreeSet::from([1, 3]));
}

#[test]
fn a_scramble_is_the_same_for_the_same_seed_and_differs_across_seeds() {
    let build = || {
        let fs = SimFs::new();
        let file = synced_file(&fs, "db", &[page(1); 4]);
        file.write(0, [page(2); 6].as_flattened()).unwrap();
        fs
    };
    let image = |seed| pages(&build().crash(Crash::Scramble { seed }), "db");
    assert_eq!(image(7), image(7));
    let distinct: BTreeSet<Vec<Block>> = (0..20).map(image).collect();
    assert!(distinct.len() > 10);
}

#[test]
fn an_error_once_fails_only_the_chosen_operation() {
    let fs = SimFs::new();
    let file = fs.create("db", size()).unwrap();
    let start = fs.operations();
    fs.fail_after(2, Failure::Once);
    file.write(0, [page(1)].as_flattened()).unwrap();
    file.write(1, [page(2)].as_flattened()).unwrap();
    assert!(file.write(2, [page(3)].as_flattened()).is_err());
    file.write(3, [page(4)].as_flattened()).unwrap();
    assert_eq!(fs.operations() - start, 4);
    assert_eq!(pages(&fs, "db"), [page(1), page(2), page(0), page(4)]);
}

#[test]
fn an_error_from_then_on_fails_every_later_operation_until_stopped() {
    let fs = SimFs::new();
    let file = fs.create("db", size()).unwrap();
    fs.fail_after(1, Failure::FromThenOn);
    file.write(0, [page(1)].as_flattened()).unwrap();
    assert!(file.write(1, [page(2)].as_flattened()).is_err());
    assert!(file.block_count().is_err());
    assert!(fs.names().is_err());
    assert!(fs.open("db", size()).is_err());
    fs.stop_failing();
    assert_eq!(pages(&fs, "db"), [page(1)]);
}

#[test]
fn a_failed_write_changes_nothing() {
    let fs = SimFs::new();
    let file = synced_file(&fs, "db", &[page(1)]);
    fs.fail_after(0, Failure::Once);
    assert!(file.write(0, [page(2), page(3)].as_flattened()).is_err());
    assert_eq!(pages(&fs, "db"), [page(1)]);
    assert_eq!(pages(&fs.crash(Crash::KeepUnsynced), "db"), [page(1)]);
}

#[test]
fn an_error_aimed_at_one_file_counts_and_fails_only_that_file() {
    let fs = SimFs::new();
    let mirror_a = fs.create("a", size()).unwrap();
    let mirror_b = fs.create("b", size()).unwrap();
    fs.fail_file_after("b", 1, Failure::FromThenOn).unwrap();
    mirror_a.write(0, [page(1)].as_flattened()).unwrap();
    mirror_b.write(0, [page(1)].as_flattened()).unwrap();
    mirror_a.write(1, [page(2)].as_flattened()).unwrap();
    mirror_a.sync().unwrap();
    assert!(mirror_b.write(1, [page(2)].as_flattened()).is_err());
    assert!(mirror_b.sync().is_err());
    mirror_a.write(2, [page(3)].as_flattened()).unwrap();
    fs.sync_names().unwrap();
    assert!(fs.fail_file_after("missing", 0, Failure::Once).is_err());
}

#[test]
fn a_failed_sync_fails_every_later_sync_of_that_file() {
    let fs = SimFs::new();
    let file = synced_file(&fs, "db", &[page(1)]);
    let other = synced_file(&fs, "other", &[page(1)]);
    file.write(0, [page(2)].as_flattened()).unwrap();
    fs.fail_file_after("db", 0, Failure::Once).unwrap();
    assert!(file.sync().is_err());
    assert!(file.sync().is_err());
    file.write(0, [page(3)].as_flattened()).unwrap();
    assert!(file.sync().is_err());
    other.sync().unwrap();
    assert_eq!(pages(&fs.crash(Crash::DropUnsynced), "db"), [page(1)]);
    let reopened = fs.crash(Crash::DropUnsynced);
    reopened.open("db", size()).unwrap().sync().unwrap();
}

#[test]
fn a_failed_sync_of_the_names_fails_every_later_one() {
    let fs = SimFs::new();
    fs.create("db", size()).unwrap();
    fs.fail_after(0, Failure::Once);
    assert!(fs.sync_names().is_err());
    assert!(fs.sync_names().is_err());
    assert!(fs.crash(Crash::DropUnsynced).names().unwrap().is_empty());
}

#[test]
fn every_operation_is_counted() {
    let fs = SimFs::new();
    let file = fs.create("db", size()).unwrap();
    file.write(0, [page(1)].as_flattened()).unwrap();
    file.read(0, [page(0)].as_flattened_mut()).unwrap();
    file.block_count().unwrap();
    file.set_block_count(2).unwrap();
    file.sync().unwrap();
    fs.open("db", size()).unwrap();
    fs.rename("db", "moved").unwrap();
    fs.names().unwrap();
    fs.sync_names().unwrap();
    fs.remove("moved").unwrap();
    assert_eq!(fs.operations(), 11);
}

#[test]
fn an_error_at_each_operation_of_a_workload_is_reached_exactly_once() {
    let workload = |fs: &SimFs| -> io::Result<()> {
        let file = fs.create("db", size())?;
        file.write(0, [page(1), page(2)].as_flattened())?;
        file.sync()?;
        fs.sync_names()?;
        fs.rename("db", "final")?;
        fs.sync_names()
    };
    let clean = SimFs::new();
    workload(&clean).unwrap();
    let total = clean.operations();
    for failing in 0..total {
        let fs = SimFs::new();
        fs.fail_after(failing, Failure::Once);
        assert!(workload(&fs).is_err(), "operation {failing} did not fail");
        assert_eq!(fs.operations(), failing + 1);
    }
}

#[test]
fn a_scramble_tears_each_block_of_a_large_block_file_at_sectors() {
    let large = BlockSize::new(1 << 16).unwrap();
    let sectors_per_block = large.bytes() / SECTOR_SIZE;
    let mut seen = BTreeSet::new();
    for seed in 0..SEEDS {
        let fs = SimFs::new();
        let file = fs.create("extents", large).unwrap();
        file.write(0, &vec![1; 2 * large.bytes()]).unwrap();
        file.sync().unwrap();
        fs.sync_names().unwrap();
        file.write(0, &vec![2; 2 * large.bytes()]).unwrap();

        let crashed = fs.crash(Crash::Scramble { seed });
        let image = crashed.open("extents", large).unwrap();
        assert_eq!(image.block_count().unwrap(), 2);
        let mut bytes = vec![0; 2 * large.bytes()];
        image.read(0, &mut bytes).unwrap();
        for block in bytes.chunks(large.bytes()) {
            let new = block
                .as_chunks::<SECTOR_SIZE>()
                .0
                .iter()
                .filter(|sector| sector.iter().all(|byte| *byte == 2))
                .count();
            seen.insert(match new {
                0 => "old or garbage",
                count if count == sectors_per_block => "new",
                _ => "torn",
            });
        }
    }
    assert_eq!(seen, BTreeSet::from(["new", "old or garbage", "torn"]));
}

#[test]
fn files_of_different_block_sizes_live_side_by_side() {
    let fs = SimFs::new();
    let small = BlockSize::new(SECTOR_SIZE).unwrap();
    let segment = fs.create("wal-0", small).unwrap();
    let database = fs.create("db", size()).unwrap();
    segment.write(3, &[7; SECTOR_SIZE]).unwrap();
    database.write(1, &[8; BLOCK]).unwrap();
    assert_eq!(segment.block_count().unwrap(), 4);
    assert_eq!(database.block_count().unwrap(), 2);
    assert_eq!(fs.open("wal-0", size()).unwrap().block_count().unwrap(), 0);
}

#[test]
fn a_lock_keeps_other_handles_out_until_its_handle_closes() {
    let fs = SimFs::new();
    let size = BlockSize::new(512).unwrap();
    let first = fs.create("locked", size).unwrap();
    first.lock().unwrap();
    first.lock().unwrap();
    let second = fs.open("locked", size).unwrap();
    assert_eq!(second.lock().unwrap_err().kind(), io::ErrorKind::WouldBlock);
    drop(first);
    second.lock().unwrap();
    assert!(
        fs.crash(Crash::KeepUnsynced)
            .open("locked", size)
            .unwrap()
            .lock()
            .is_ok()
    );
}
