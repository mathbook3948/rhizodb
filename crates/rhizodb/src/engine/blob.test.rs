use proptest::prelude::*;

use super::*;
use crate::engine::layout::ExtentId;

const SEED: u64 = 3;

fn run(blocks: u32) -> RunRef {
    RunRef {
        at: BlockRef::new(ExtentId(4), 10).unwrap(),
        blocks,
        lsn: 6,
    }
}

fn spec<'a>(payload: &'a [u8], owner: &'a [u8], next: RunRef) -> RunSpec<'a> {
    RunSpec {
        role: Role::Value,
        table: TableId(0x100),
        at: run(1).at,
        lsn: 6,
        generation: 2,
        total: payload.len() as u64 + 10,
        next,
        next_slot: if next.at.is_null() { 0 } else { 11 },
        owner,
        payload,
    }
}

#[test]
fn a_run_round_trips_with_its_owner() {
    let payload = vec![5u8; 9000];
    let next = RunRef {
        at: BlockRef::new(ExtentId(5), 0).unwrap(),
        blocks: 2,
        lsn: 6,
    };
    let owner = vec![b'o'; 100];
    let blocks = spec(&payload, &owner, next).encode(SEED).unwrap();
    assert_eq!(blocks.len(), run_blocks(payload.len()) as usize * BLOCK);
    let (header, decoded) =
        decode_run(SEED, &blocks, run(run_blocks(payload.len())), Role::Value).unwrap();
    assert_eq!(header.kind, PageKind::ValueRun);
    assert_eq!(decoded.payload, payload.as_slice());
    assert_eq!(decoded.next, next);
    assert_eq!(decoded.next_slot, 11);
    assert_eq!(decoded.total, 9010);
    assert_eq!(decoded.owner_length, 100);
    assert_eq!(decoded.owner_head, &owner[..64]);
    assert_eq!(decoded.owner_hash, owner_of(&owner).2);
}

#[test]
fn a_run_read_with_the_wrong_role_length_or_lsn_is_corrupt() {
    let payload = vec![1u8; 100];
    let blocks = spec(&payload, b"k", RunRef::default())
        .encode(SEED)
        .unwrap();
    assert!(
        decode_run(SEED, &blocks, run(1), Role::Key)
            .unwrap_err()
            .is_corrupt()
    );
    assert!(
        decode_run(SEED, &blocks, run(1), Role::Catalog)
            .unwrap_err()
            .is_corrupt()
    );
    assert!(
        decode_run(SEED, &blocks, run(2), Role::Value)
            .unwrap_err()
            .is_corrupt()
    );
    assert!(
        decode_run(SEED, &blocks, RunRef { lsn: 7, ..run(1) }, Role::Value)
            .unwrap_err()
            .is_corrupt()
    );
    assert!(decode_run(SEED, &blocks, run(1), Role::Value).is_ok());
}

#[test]
fn every_flipped_bit_of_a_run_is_caught() {
    let payload = vec![9u8; 5000];
    let blocks = spec(&payload, b"key", RunRef::default())
        .encode(SEED)
        .unwrap();
    for byte in (0..blocks.len()).step_by(7) {
        let mut damaged = blocks.clone();
        damaged[byte] ^= 0x04;
        assert!(
            decode_run(SEED, &damaged, run(2), Role::Value).is_err(),
            "byte {byte}"
        );
    }
}

#[test]
fn items_split_into_runs_of_whole_extents() {
    assert_eq!(split_runs(0), [0]);
    assert_eq!(split_runs(10), [10]);
    assert_eq!(split_runs(RUN_PAYLOAD_LIMIT), [RUN_PAYLOAD_LIMIT]);
    assert_eq!(split_runs(RUN_PAYLOAD_LIMIT + 1), [RUN_PAYLOAD_LIMIT, 1]);
    assert_eq!(split_runs(2 * RUN_PAYLOAD_LIMIT), [RUN_PAYLOAD_LIMIT; 2]);
    assert_eq!(run_blocks(RUN_PAYLOAD_LIMIT), EXTENT_BLOCKS);
    assert_eq!(run_blocks(0), 1);
}

proptest! {
    #[test]
    fn any_resealed_run_decodes_or_fails_without_panicking(bytes in proptest::collection::vec(any::<u8>(), BLOCK)) {
        let mut blocks = bytes;
        blocks[0] = 3;
        PageHeader::open(SEED, &blocks, run(1).at).ok();
        let header = PageHeader {
            kind: PageKind::ValueRun,
            format: 1,
            table: TableId(0x100),
            level: 0,
            flags: 0,
            cells: 0,
            free: 0,
            at: run(1).at,
            lsn: 6,
            generation: 1,
        };
        header.seal(SEED, &mut blocks).unwrap();
        let _ = decode_run(SEED, &blocks, run(1), Role::Value);
    }
}
