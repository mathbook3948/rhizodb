use super::*;
use crate::engine::blob::RunRef;
use crate::engine::layout::ExtentId;
use crate::engine::layout::TableId;
use crate::engine::page::PageKind;

fn page(at: BlockRef) -> CachedPage {
    let header = PageHeader {
        kind: PageKind::Leaf,
        format: 1,
        table: TableId(0x100),
        level: 0,
        flags: 0,
        cells: 1,
        free: 0,
        at,
        lsn: 1,
        generation: 1,
    };
    Arc::new(CheckedPage {
        header,
        bytes: vec![(at.raw() % 251) as u8; 8].into_boxed_slice(),
    })
}

fn at(block: u32) -> BlockRef {
    BlockRef::new(ExtentId(1 + u64::from(block / 1000)), block % 1000).unwrap()
}

fn blob(block: u32, lsn: u64, length: usize) -> BlobRef {
    BlobRef {
        first: RunRef {
            at: at(block),
            blocks: 1,
            lsn,
        },
        length: length as u64,
        checksum: lsn,
    }
}

fn value(length: usize, fill: u8) -> Arc<[u8]> {
    vec![fill; length].into()
}

#[test]
fn a_cached_page_comes_back_until_evicted() {
    let cache = Cache::new(64 * BLOCK);
    assert!(cache.get(at(1)).is_none());
    cache.insert(at(1), page(at(1)), Access::Point);
    assert_eq!(
        cache.get(at(1)).unwrap().bytes[0],
        (at(1).raw() % 251) as u8
    );
    cache.insert(at(1), page(at(2)), Access::Point);
    assert_eq!(
        cache.get(at(1)).unwrap().bytes[0],
        (at(1).raw() % 251) as u8
    );
}

#[test]
fn the_cache_stays_within_its_capacity_and_keeps_used_pages() {
    let cache = Cache::new(SHARDS * 4 * BLOCK);
    for block in 0..1000 {
        cache.insert(at(block), page(at(block)), Access::Point);
        assert!(cache.len() <= SHARDS * 4);
        if let Some(found) = cache.get(at(block)) {
            assert_eq!(found.bytes[0], (at(block).raw() % 251) as u8);
        }
    }
    let hot = at(999);
    for block in 0..200 {
        assert!(
            cache.get(hot).is_some(),
            "a page in use was evicted at {block}"
        );
        cache.insert(at(2000 + block), page(at(2000 + block)), Access::Point);
    }
}

#[test]
fn a_cached_value_comes_back_only_for_the_pointer_it_was_read_for() {
    let cache = Cache::new(1 << 20);
    cache.insert_value(blob(1, 5, 2000), value(2000, 7), Access::Point);
    assert_eq!(*cache.value(&blob(1, 5, 2000)).unwrap(), *value(2000, 7));
    assert!(cache.value(&blob(1, 6, 2000)).is_none());
    assert!(cache.get(at(1)).is_none(), "a value read back as a page");
    cache.remove(at(1));
    assert!(cache.value(&blob(1, 5, 2000)).is_none());
    assert_eq!(cache.bytes(), 0);
}

#[test]
fn pages_and_values_together_stay_within_the_budget_and_keep_what_is_used() {
    let budget = SHARDS * 8 * BLOCK;
    let cache = Cache::new(budget);
    let hot = blob(999_999, 1, 3000);
    cache.insert_value(hot, value(3000, 1), Access::Point);
    for block in 0..5000 {
        if block % 2 == 0 {
            cache.insert(at(block), page(at(block)), Access::Point);
        } else {
            let length = 1100 + block as usize % 7000;
            cache.insert_value(
                blob(block, 1, length),
                value(length, block as u8),
                Access::Point,
            );
        }
        assert!(
            cache.bytes() <= budget,
            "the cache holds {} bytes",
            cache.bytes()
        );
        assert!(
            cache.value(&hot).is_some(),
            "a value in use was evicted at {block}"
        );
    }
}

#[test]
fn a_value_larger_than_a_quarter_of_its_shard_is_not_kept() {
    let cache = Cache::new(SHARDS * 4 * BLOCK);
    cache.insert_value(blob(1, 1, BLOCK + 1), value(BLOCK + 1, 1), Access::Point);
    assert!(cache.value(&blob(1, 1, BLOCK + 1)).is_none());
    cache.insert_value(blob(2, 1, BLOCK), value(BLOCK, 1), Access::Point);
    assert!(cache.value(&blob(2, 1, BLOCK)).is_some());
}

#[test]
fn rewriting_and_reading_one_block_again_and_again_keeps_the_clock_bounded() {
    let cache = Cache::new(1 << 30);
    for round in 0..10_000 {
        cache.remove(at(1));
        cache.insert_value(blob(1, round, 2000), value(2000, 1), Access::Point);
    }
    assert_eq!(cache.len(), 1);
    assert!(cache.places() <= 2 + CLOCK_SLACK);
}

fn read_page(cache: &Cache, block: u32) -> bool {
    let found = cache.get(at(block)).is_some();
    if !found {
        cache.insert(at(block), page(at(block)), Access::Point);
    }
    found
}

#[test]
fn a_new_working_set_takes_the_place_of_one_no_longer_read() {
    let cache = Cache::new(SHARDS * 64 * BLOCK);
    let old = SHARDS as u32 * 48;
    for _ in 0..3 {
        for block in 0..old {
            read_page(&cache, block * 7 % old);
        }
    }
    let new = SHARDS as u32 * 32;
    let mut hits = 0;
    for _ in 0..4 {
        hits = (0..new)
            .filter(|block| read_page(&cache, 100_000 + block * 7 % new))
            .count();
    }
    assert_eq!(hits, new as usize);
}

#[test]
fn a_scan_takes_only_free_room_and_keeps_what_it_holds_for_the_next_scan() {
    let cache = Cache::new(SHARDS * 16 * BLOCK);
    let hot = SHARDS as u32 * 6;
    for _ in 0..2 {
        for block in 0..hot {
            read_page(&cache, block);
        }
    }
    let scan = |from: u32, values: u32| {
        (from..from + values)
            .filter(|&block| {
                let blob = blob(block, 1, 2000);
                let found = cache.value(&blob).is_some();
                if !found {
                    cache.insert_value(blob, value(2000, 1), Access::Scan);
                }
                found
            })
            .count()
    };
    let fits = SHARDS as u32 * 2;
    assert_eq!(scan(100_000, fits), 0);
    assert_eq!(scan(100_000, fits), fits as usize);
    assert_eq!(scan(500_000, SHARDS as u32 * 200), 0);
    assert!(
        (0..hot).all(|block| cache.get(at(block)).is_some()),
        "a scan pushed out a page in use"
    );
    assert_eq!(
        scan(100_000, fits),
        fits as usize,
        "a scan pushed out a scan's value"
    );
}
