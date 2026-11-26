use proptest::prelude::*;

use super::*;
use crate::engine::codec::put_u16;
use crate::engine::codec::put_u32;
use crate::engine::codec::put_u64;
use crate::engine::extent::Purpose;
use crate::engine::layout::EXTENT_BLOCKS;

fn sample() -> Catalog {
    let mut extents = ExtentTable::default();
    extents.place(5, |_, _| {}).unwrap();
    extents.place(EXTENT_BLOCKS, |_, _| {}).unwrap();
    extents.entries.insert(
        extents.next_extent,
        ExtentEntry {
            slot: extents.next_slot,
            written: 7,
            used: Blocks::range(1, 1).with(&Blocks::range(5, 1)),
            state: ExtentState::Sealed,
            purpose: Purpose::Meta,
            scheme: 0,
            opened_term: 0,
        },
    );
    extents.next_extent = ExtentId(extents.next_extent.0 + 1);
    extents.next_slot += 1;
    let mut roots = BTreeMap::new();
    roots.insert(
        TableId(0x100),
        Root {
            page: ChildRef {
                at: BlockRef::new(ExtentId(1), 4).unwrap(),
                lsn: 9,
            },
            level: 2,
            entries: 1234,
        },
    );
    Catalog {
        roots,
        extents,
        live_counts: None,
    }
}

#[test]
fn a_catalog_round_trips() {
    let catalog = sample();
    assert_eq!(
        Catalog::decode(&catalog.encode().unwrap()).unwrap(),
        catalog
    );
    let empty = Catalog::default();
    assert_eq!(Catalog::decode(&empty.encode().unwrap()).unwrap(), empty);
}

#[test]
fn a_catalog_with_an_unknown_table_or_format_is_unsupported() {
    let mut catalog = sample();
    let root = catalog.roots[&TableId(0x100)];
    catalog.roots.insert(TableId(0x1FF), root);
    assert!(matches!(
        Catalog::decode(&catalog.encode().unwrap()),
        Err(EngineError::Unsupported(_))
    ));
    let mut bytes = sample().encode().unwrap();
    bytes[0] = 3;
    assert!(matches!(
        Catalog::decode(&bytes),
        Err(EngineError::Unsupported(_))
    ));
}

#[test]
fn every_cut_of_a_catalog_is_corrupt() {
    let bytes = sample().encode().unwrap();
    for cut in 1..bytes.len() {
        assert!(Catalog::decode(&bytes[..cut]).is_err(), "cut at {cut}");
    }
    let mut longer = bytes.clone();
    longer.push(0);
    assert!(Catalog::decode(&longer).unwrap_err().is_corrupt());
}

fn counts_only(catalog: &Catalog) -> Vec<u8> {
    let mut out = vec![1];
    put_u64(&mut out, catalog.extents.next_slot);
    put_u64(&mut out, catalog.extents.next_extent.0);
    for open in catalog.extents.open {
        put_u64(&mut out, open.map_or(0, |id| id.0));
    }
    put_u32(&mut out, catalog.roots.len() as u32);
    for (table, root) in &catalog.roots {
        put_u16(&mut out, table.0);
        put_u64(&mut out, root.page.at.raw());
        put_u64(&mut out, root.page.lsn);
        out.push(root.level);
        put_u64(&mut out, root.entries);
    }
    put_u32(&mut out, catalog.extents.entries.len() as u32);
    for (id, entry) in &catalog.extents.entries {
        put_u64(&mut out, id.0);
        put_u64(&mut out, entry.slot);
        put_u32(&mut out, entry.written);
        put_u32(&mut out, entry.live());
        out.push(u8::from(entry.state == ExtentState::Sealed));
        out.push(entry.purpose.code());
        out.extend_from_slice(&[0, 0]);
        put_u64(&mut out, 0);
        put_u64(&mut out, 0);
    }
    out
}

#[test]
fn a_catalog_that_counts_live_blocks_only_counts_every_written_block_used_until_a_walk() {
    let catalog = sample();
    let decoded = Catalog::decode(&counts_only(&catalog)).unwrap();
    assert_eq!(decoded.roots, catalog.roots);
    let counts = decoded.live_counts.as_ref().unwrap();
    for (id, entry) in &catalog.extents.entries {
        assert_eq!(counts[id], entry.live());
        let read = &decoded.extents.entries[id];
        assert_eq!(read.used, Blocks::range(0, entry.written));
        assert!(read.free().is_empty());
    }
    assert_eq!(
        Catalog::decode(&catalog.encode().unwrap())
            .unwrap()
            .live_counts,
        None
    );
}

#[test]
fn holes_that_overlap_touch_or_pass_the_written_blocks_are_corrupt() {
    let catalog = sample();
    let bytes = catalog.encode().unwrap();
    let id = *catalog.extents.entries.keys().last().unwrap();
    let entry = catalog.extents.entries[&id];
    let holes: Vec<(u32, u32)> = entry.free().ranges().collect();
    assert_eq!(holes, [(0, 1), (2, 3), (6, 1)]);
    let at = bytes.len() - 2 - 4 * holes.len();
    assert_eq!(&bytes[at..at + 2], &(holes.len() as u16).to_le_bytes());
    for (start, length) in [(1, 1), (2, 4), (6, 2), (0, 0), (5, 1)] {
        let mut damaged = bytes.clone();
        let last = bytes.len() - 4;
        damaged[last..last + 2].copy_from_slice(&(start as u16).to_le_bytes());
        damaged[last + 2..].copy_from_slice(&(length as u16).to_le_bytes());
        assert!(
            Catalog::decode(&damaged).unwrap_err().is_corrupt(),
            "a last hole of {length} blocks at {start}"
        );
    }
}

proptest! {
    #[test]
    fn any_bytes_decode_or_fail_without_panicking(bytes in proptest::collection::vec(any::<u8>(), 0..400)) {
        let mut bytes = bytes;
        if let Some(first) = bytes.first_mut() {
            *first = 1;
        }
        let _ = Catalog::decode(&bytes);
    }
}
