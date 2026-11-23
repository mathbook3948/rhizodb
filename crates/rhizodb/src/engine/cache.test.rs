use super::*;
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
    (header, Arc::from(vec![(at.raw() % 251) as u8; 8]))
}

fn at(block: u32) -> BlockRef {
    BlockRef::new(ExtentId(1 + u64::from(block / 1000)), block % 1000).unwrap()
}

#[test]
fn a_cached_page_comes_back_until_evicted() {
    let cache = Cache::new(64);
    assert!(cache.get(at(1)).is_none());
    cache.insert(at(1), page(at(1)));
    assert_eq!(cache.get(at(1)).unwrap().1[0], (at(1).raw() % 251) as u8);
    cache.insert(at(1), page(at(2)));
    assert_eq!(cache.get(at(1)).unwrap().1[0], (at(1).raw() % 251) as u8);
}

#[test]
fn the_cache_stays_within_its_capacity_and_keeps_used_pages() {
    let cache = Cache::new(SHARDS * 4);
    for block in 0..1000 {
        cache.insert(at(block), page(at(block)));
        assert!(cache.len() <= SHARDS * 4);
        if let Some(found) = cache.get(at(block)) {
            assert_eq!(found.1[0], (at(block).raw() % 251) as u8);
        }
    }
    let hot = at(999);
    for block in 0..200 {
        assert!(
            cache.get(hot).is_some(),
            "a page in use was evicted at {block}"
        );
        cache.insert(at(2000 + block), page(at(2000 + block)));
    }
}
