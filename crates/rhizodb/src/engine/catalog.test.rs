use proptest::prelude::*;

use super::*;
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
            live: 2,
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
    Catalog { roots, extents }
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
    bytes[0] = 2;
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
