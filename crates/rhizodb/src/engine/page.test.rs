use proptest::prelude::*;

use super::*;
use crate::engine::layout::ExtentId;

const SEED: u64 = 0x5EED;

fn at() -> BlockRef {
    BlockRef::new(ExtentId(9), 33).unwrap()
}

fn header() -> PageHeader {
    PageHeader {
        kind: PageKind::Leaf,
        format: 1,
        table: TableId(0x105),
        level: 0,
        flags: 0,
        cells: 3,
        free: 200,
        at: at(),
        lsn: 77,
        generation: 4,
    }
}

fn sealed(blocks: usize) -> Vec<u8> {
    let mut page: Vec<u8> = (0..blocks * BLOCK).map(|index| (index * 7) as u8).collect();
    header().seal(SEED, &mut page).unwrap();
    page
}

#[test]
fn a_page_header_round_trips() {
    for blocks in [1, 3] {
        let page = sealed(blocks);
        assert_eq!(PageHeader::open(SEED, &page, at()).unwrap(), header());
    }
}

#[test]
fn every_flipped_bit_of_a_page_is_caught() {
    let page = sealed(1);
    for byte in 0..BLOCK {
        for bit in 0..8 {
            let mut damaged = page.clone();
            damaged[byte] ^= 1 << bit;
            assert!(
                PageHeader::open(SEED, &damaged, at())
                    .unwrap_err()
                    .is_corrupt(),
                "a flip of bit {bit} of byte {byte} went unnoticed"
            );
        }
    }
}

#[test]
fn a_flip_in_any_block_of_a_long_page_is_caught() {
    let page = sealed(4);
    for byte in (0..page.len()).step_by(509) {
        let mut damaged = page.clone();
        damaged[byte] ^= 0x10;
        assert!(PageHeader::open(SEED, &damaged, at()).is_err());
    }
}

#[test]
fn a_page_read_from_another_block_or_database_is_corrupt() {
    let page = sealed(1);
    let elsewhere = BlockRef::new(ExtentId(9), 34).unwrap();
    assert!(
        PageHeader::open(SEED, &page, elsewhere)
            .unwrap_err()
            .is_corrupt()
    );
    assert!(
        PageHeader::open(SEED + 1, &page, at())
            .unwrap_err()
            .is_corrupt()
    );
    assert!(PageHeader::open(SEED, &vec![0; BLOCK], at()).is_err());
}

#[test]
fn a_page_must_be_whole_blocks() {
    let mut short = vec![0; BLOCK - 1];
    assert!(header().seal(SEED, &mut short).is_err());
    assert!(PageHeader::open(SEED, &short, at()).is_err());
    assert!(PageHeader::open(SEED, &[], at()).is_err());
}

#[test]
fn the_expected_kind_and_lsn_are_checked() {
    let header = header();
    assert!(header.expect(PageKind::Leaf, 77).is_ok());
    assert!(
        header
            .expect(PageKind::Branch, 77)
            .unwrap_err()
            .is_corrupt()
    );
    assert!(header.expect(PageKind::Leaf, 76).unwrap_err().is_corrupt());
}

#[test]
fn unknown_kinds_and_reserved_bytes_are_corrupt() {
    for (at_byte, value) in [(0usize, 0u8), (0, 9), (10, 1), (37, 1)] {
        let mut page = sealed(1);
        page[at_byte] = value;
        let sum = page_checksum(SEED, &page);
        page[CHECKSUM_AT..PAGE_HEADER].copy_from_slice(&sum.to_le_bytes());
        assert!(
            PageHeader::open(SEED, &page, at())
                .unwrap_err()
                .is_corrupt()
        );
    }
}

#[test]
fn the_seed_folds_both_halves_of_the_database_id() {
    assert_ne!(page_seed(1), page_seed(1 << 64 | 1));
    assert_eq!(page_seed(0), 0);
}

proptest! {
    #[test]
    #[ignore = "left out of mise run check, run by mise run random"]
    fn any_page_opens_or_fails_without_panicking(
        bytes in proptest::collection::vec(any::<u8>(), BLOCK),
        reseal in any::<bool>(),
    ) {
        let mut page = bytes;
        if reseal {
            page[12..20].copy_from_slice(&at().raw().to_le_bytes());
            let sum = page_checksum(SEED, &page);
            page[CHECKSUM_AT..PAGE_HEADER].copy_from_slice(&sum.to_le_bytes());
        }
        let _ = PageHeader::open(SEED, &page, at());
    }
}
