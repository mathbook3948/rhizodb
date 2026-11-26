use std::collections::BTreeMap;

use super::*;
use proptest::prelude::*;

const BLOCK: u64 = 5;

fn first() -> u64 {
    BLOCK << BLOCK_SHIFT
}

type Edit = (u64, Option<(Vec<u32>, u32)>);

fn postings() -> impl Strategy<Value = (Vec<u32>, u32)> {
    (
        prop::collection::btree_set(0u32..10_000, 1..6),
        0u32..100_000,
    )
        .prop_map(|(positions, length)| (positions.into_iter().collect(), length))
}

fn edits() -> impl Strategy<Value = Vec<Edit>> {
    prop::collection::vec(
        (
            0u64..(1 << BLOCK_SHIFT),
            prop::option::weighted(0.8, postings()),
        ),
        0..200,
    )
}

fn read(block: &Block) -> Vec<Posting> {
    block
        .bytes()
        .map(|bytes| {
            Entries::new(BLOCK, bytes)
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        })
        .unwrap_or_default()
}

proptest! {
    #[test]
    #[ignore = "left out of mise run check, run by mise run random"]
    fn a_block_reads_back_what_its_edits_left(edits in edits()) {
        let mut block = Block::default();
        let mut model = BTreeMap::new();
        for (offset, posting) in edits {
            let element = first() + offset;
            match posting {
                Some((positions, length)) => {
                    block.insert(BLOCK, element, &positions, length).unwrap();
                    model.insert(element, (positions, length));
                }
                None => {
                    block.remove(BLOCK, element).unwrap();
                    model.remove(&element);
                }
            }
        }
        let expected: Vec<Posting> = model
            .into_iter()
            .map(|(element, (positions, length))| Posting {
                element,
                frequency: u32::try_from(positions.len()).unwrap(),
                length,
                positions,
            })
            .collect();
        prop_assert_eq!(read(&block), expected);
        if let Some(bytes) = block.bytes() {
            let loaded = Block::load(BLOCK, bytes).unwrap();
            prop_assert_eq!(read(&loaded), read(&block));
            let mut counts = Entries::new(BLOCK, bytes);
            for posting in read(&block) {
                prop_assert_eq!(
                    counts.next_counts().unwrap().unwrap(),
                    (posting.element, posting.frequency, posting.length)
                );
            }
            prop_assert!(counts.next_counts().is_none());
        }
    }

    #[test]
    #[ignore = "left out of mise run check, run by mise run random"]
    fn a_block_built_in_any_order_has_the_bytes_of_one_built_in_order(
        edits in prop::collection::btree_map(0u64..(1 << BLOCK_SHIFT), postings(), 1..40),
        seed in any::<u64>(),
    ) {
        let mut ordered = Block::default();
        for (offset, (positions, length)) in &edits {
            ordered.insert(BLOCK, first() + offset, positions, *length).unwrap();
        }
        let mut shuffled: Vec<_> = edits.iter().collect();
        shuffled.sort_by_key(|(offset, _)| (**offset).wrapping_mul(seed | 1));
        let mut unordered = Block::default();
        for (offset, (positions, length)) in shuffled {
            unordered.insert(BLOCK, first() + offset, positions, *length).unwrap();
        }
        prop_assert_eq!(ordered.bytes(), unordered.bytes());
    }

    #[test]
    #[ignore = "left out of mise run check, run by mise run random"]
    fn damaged_block_bytes_fail_or_read_without_a_panic(
        bytes in prop::collection::vec(any::<u8>(), 0..64),
    ) {
        let loaded = Block::load(BLOCK, &bytes);
        let entries: Result<Vec<_>, _> = Entries::new(BLOCK, &bytes).collect();
        prop_assert_eq!(loaded.is_ok(), entries.as_ref().is_ok_and(|read| !read.is_empty()));
    }
}

#[test]
fn a_block_with_no_posting_has_no_bytes() {
    let mut block = Block::default();
    assert_eq!(block.bytes(), None);
    block.insert(BLOCK, first() + 3, &[1, 4], 9).unwrap();
    assert!(block.bytes().is_some());
    block.remove(BLOCK, first() + 3).unwrap();
    assert_eq!(block.bytes(), None);
    assert!(Block::load(BLOCK, &[]).is_err());
}

#[test]
fn a_posting_outside_its_block_fails_to_decode() {
    let mut block = Block::default();
    block.insert(BLOCK, first(), &[0], 1).unwrap();
    let bytes = block.bytes().unwrap().to_vec();
    assert!(Block::load(BLOCK + 1, &bytes).is_ok());
    let mut past = Vec::new();
    put_varint(&mut past, 1 << BLOCK_SHIFT);
    past.extend_from_slice(&bytes[1..]);
    assert!(Block::load(BLOCK, &past).is_err());
}

#[test]
fn a_posting_cut_short_fails_to_decode() {
    let mut block = Block::default();
    block
        .insert(BLOCK, first() + 1, &[2, 300, 70_000], 12)
        .unwrap();
    let bytes = block.bytes().unwrap();
    for end in 1..bytes.len() {
        assert!(Block::load(BLOCK, &bytes[..end]).is_err(), "{end}");
    }
}

#[test]
fn elements_map_to_blocks_of_consecutive_ids() {
    assert_eq!(block_of(0), 0);
    assert_eq!(block_of((1 << BLOCK_SHIFT) - 1), 0);
    assert_eq!(block_of(1 << BLOCK_SHIFT), 1);
    assert_eq!(block_of(u64::MAX), u64::MAX >> BLOCK_SHIFT);
}
