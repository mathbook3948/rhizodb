use proptest::prelude::*;

use super::*;

fn sample() -> Header {
    Header {
        minor: FORMAT_MINOR,
        database_id: 0x0123_4567_89AB_CDEF_0011_2233_4455_6677,
        shard_id: 9,
        generation: 7,
        checkpoint_lsn: 1000,
        checkpoint_hlc: 55,
        catalog: BlockRef::new(ExtentId(4), 12).unwrap(),
        catalog_lsn: 1000,
        next_extent: ExtentId(6),
        open: [
            OpenExtent {
                id: ExtentId(3),
                slot: 2,
                written: 1000,
            },
            OpenExtent {
                id: ExtentId(5),
                slot: 4,
                written: 1,
            },
            OpenExtent::default(),
            OpenExtent {
                id: ExtentId(4),
                slot: 3,
                written: EXTENT_BLOCKS,
            },
        ],
        wal: WalIdentity {
            first_segment: 12,
            salt: 0xDEAD_BEEF,
        },
        clean: true,
        staged_lsn: 0,
        replication: Replication {
            incarnation: 0xFEED_F00D,
            replica: true,
            seen_term: 4,
            checkpoint_chain: 0xC4A1,
            origin: 0xD00D,
        },
    }
}

fn resealed(mut block: Vec<u8>) -> Vec<u8> {
    let sum = checksum(0, &[&block[..CHECKSUM_AT]]);
    block[CHECKSUM_AT..].copy_from_slice(&sum.to_le_bytes());
    block
}

#[test]
fn a_header_round_trips_and_fills_one_block() {
    let header = sample();
    let block = header.encode();
    assert_eq!(block.len(), BLOCK);
    assert_eq!(&block[..8], b"RHIZODB\0");
    assert_eq!(Header::decode(&block).unwrap(), header);
    let unclean = Header {
        clean: false,
        ..header
    };
    assert_eq!(Header::decode(&unclean.encode()).unwrap(), unclean);
}

#[test]
fn a_staged_header_keeps_the_log_start_and_names_the_tree_lsn_past_it() {
    let staged = Header {
        clean: false,
        staged_lsn: 1500,
        catalog_lsn: 1500,
        ..sample()
    };
    let decoded = Header::decode(&staged.encode()).unwrap();
    assert_eq!(decoded, staged);
    assert_eq!(decoded.checkpoint_lsn, 1000);
    assert_eq!(decoded.tree_lsn(), 1500);
    assert_eq!(sample().tree_lsn(), 1000);
    for bad in [
        Header {
            clean: true,
            ..staged
        },
        Header {
            staged_lsn: 1000,
            catalog_lsn: 900,
            ..staged
        },
        Header {
            catalog_lsn: 1501,
            ..staged
        },
    ] {
        assert!(
            Header::decode(&bad.encode()).unwrap_err().is_corrupt(),
            "{bad:?}"
        );
    }
}

#[test]
fn every_flipped_bit_of_a_header_is_caught() {
    let block = sample().encode();
    for byte in 0..BLOCK {
        for bit in 0..8 {
            let mut damaged = block.clone();
            damaged[byte] ^= 1 << bit;
            assert!(
                Header::decode(&damaged).is_err(),
                "a flip of bit {bit} of byte {byte} went unnoticed"
            );
        }
    }
}

#[test]
fn a_truncated_or_zeroed_header_is_corrupt() {
    let block = sample().encode();
    for length in [0, 8, 100, BLOCK - 1] {
        assert!(Header::decode(&block[..length]).unwrap_err().is_corrupt());
    }
    assert!(Header::decode(&vec![0; BLOCK]).unwrap_err().is_corrupt());
}

#[test]
fn a_newer_format_is_unsupported_and_not_corrupt() {
    for (major, minor) in [(2u16, 0u16), (0, 0), (1, FORMAT_MINOR + 1)] {
        let mut block = sample().encode();
        block[8..10].copy_from_slice(&major.to_le_bytes());
        block[10..12].copy_from_slice(&minor.to_le_bytes());
        let error = Header::decode(&resealed(block)).unwrap_err();
        assert!(matches!(error, EngineError::Unsupported(_)), "{error}");
    }
}

#[test]
fn another_block_or_extent_size_is_unsupported() {
    for at in [12, 16] {
        let mut block = sample().encode();
        block[at] ^= 1;
        assert!(matches!(
            Header::decode(&resealed(block)).unwrap_err(),
            EngineError::Unsupported(_)
        ));
    }
}

#[test]
fn an_unknown_flag_is_unsupported() {
    let mut block = sample().encode();
    let flags_at = 208;
    assert_eq!(&block[flags_at..flags_at + 4], &1u32.to_le_bytes());
    block[flags_at + 1] = 1;
    assert!(matches!(
        Header::decode(&resealed(block)).unwrap_err(),
        EngineError::Unsupported(_)
    ));
}

#[test]
fn inconsistent_fields_with_a_valid_checksum_are_corrupt() {
    let bad = [
        Header {
            generation: 0,
            ..sample()
        },
        Header {
            next_extent: ExtentId::NONE,
            ..sample()
        },
        Header {
            catalog_lsn: 1001,
            ..sample()
        },
    ];
    for header in bad {
        assert!(Header::decode(&header.encode()).unwrap_err().is_corrupt());
    }
    let mut open = sample();
    open.open[0].written = EXTENT_BLOCKS + 1;
    assert!(Header::decode(&open.encode()).unwrap_err().is_corrupt());
    let mut beyond = sample();
    beyond.open[0].id = ExtentId(6);
    assert!(Header::decode(&beyond.encode()).unwrap_err().is_corrupt());
    let mut empty = sample();
    empty.open[2].written = 1;
    assert!(Header::decode(&empty.encode()).unwrap_err().is_corrupt());
    let mut catalog = sample().encode();
    catalog[72..80].copy_from_slice(&7u64.to_le_bytes());
    assert!(Header::decode(&resealed(catalog)).unwrap_err().is_corrupt());
    let mut reserved = sample().encode();
    reserved[300] = 1;
    assert!(
        Header::decode(&resealed(reserved))
            .unwrap_err()
            .is_corrupt()
    );
}

#[test]
fn a_slot_must_hold_a_generation_of_its_parity() {
    let block = sample().encode();
    assert!(decode_slot(&block, 1).is_ok());
    assert!(decode_slot(&block, 0).unwrap_err().is_corrupt());
}

#[test]
fn the_valid_slot_of_the_higher_generation_wins() {
    let older = sample();
    let newer = Header {
        generation: 8,
        checkpoint_lsn: 2000,
        ..older
    };
    let corrupt = || Err(EngineError::corrupt("torn"));
    assert_eq!(choose(Ok(newer), Ok(older)).unwrap(), newer);
    assert_eq!(choose(Ok(older), Ok(newer)).unwrap(), newer);
    assert_eq!(choose(corrupt(), Ok(older)).unwrap(), older);
    assert_eq!(choose(Ok(newer), corrupt()).unwrap(), newer);
    assert!(choose(corrupt(), corrupt()).unwrap_err().is_corrupt());
}

#[test]
fn slots_that_disagree_are_corrupt_and_a_newer_slot_refuses_the_file() {
    let header = sample();
    let other = Header {
        database_id: 1,
        generation: 8,
        ..header
    };
    assert!(choose(Ok(header), Ok(other)).unwrap_err().is_corrupt());
    assert!(choose(Ok(header), Ok(header)).unwrap_err().is_corrupt());
    let newer = || Err(EngineError::Unsupported("2.0".to_owned()));
    assert!(matches!(
        choose(Ok(header), newer()),
        Err(EngineError::Unsupported(_))
    ));
    assert!(matches!(
        choose(Err(EngineError::corrupt("torn")), newer()),
        Err(EngineError::Unsupported(_))
    ));
}

fn header_strategy() -> impl Strategy<Value = Header> {
    (
        any::<u128>(),
        any::<u64>(),
        1u64..u64::MAX,
        any::<u64>(),
        any::<u64>(),
        any::<bool>(),
        any::<[u64; 2]>(),
        (2u64..1 << 40),
        proptest::array::uniform4((0u64..2, any::<u64>(), 0..=EXTENT_BLOCKS)),
    )
        .prop_map(
            |(database_id, shard_id, generation, lsn, hlc, clean, wal, next, open)| {
                let mut header = Header {
                    minor: FORMAT_MINOR,
                    database_id,
                    shard_id,
                    generation,
                    checkpoint_lsn: lsn,
                    checkpoint_hlc: hlc,
                    catalog: BlockRef::NULL,
                    catalog_lsn: lsn / 2,
                    next_extent: ExtentId(next),
                    open: [OpenExtent::default(); OPEN_EXTENTS],
                    wal: WalIdentity {
                        first_segment: wal[0],
                        salt: wal[1],
                    },
                    staged_lsn: if clean || lsn > u64::MAX - 1000 {
                        0
                    } else {
                        lsn + 1 + hlc % 1000
                    },
                    clean,
                    replication: Replication {
                        incarnation: u128::from(hlc) << 3,
                        replica: clean,
                        seen_term: lsn / 3,
                        checkpoint_chain: hlc ^ lsn,
                        origin: u128::from(lsn) << 64,
                    },
                };
                for (entry, (present, slot, written)) in header.open.iter_mut().zip(open) {
                    if present == 1 {
                        *entry = OpenExtent {
                            id: ExtentId(next - 1),
                            slot,
                            written,
                        };
                    }
                }
                header
            },
        )
}

proptest! {
    #[test]
    fn any_header_round_trips(header in header_strategy()) {
        prop_assert_eq!(Header::decode(&header.encode()).unwrap(), header);
    }

    #[test]
    fn any_block_decodes_or_fails_without_panicking(
        bytes in proptest::collection::vec(any::<u8>(), BLOCK),
        magic in any::<bool>(),
    ) {
        let mut block = bytes;
        if magic {
            block[..8].copy_from_slice(&MAGIC);
            block = resealed(block);
        }
        let _ = Header::decode(&block);
    }
}

#[test]
fn the_log_kept_for_peers_round_trips_beside_the_header_and_reads_as_none_in_older_slots() {
    let retained = Retained {
        from: 42,
        peers: (0..PEER_LIMIT)
            .map(|index| (vec![index as u8; 64], index as u64 * 1000))
            .collect(),
    };
    let block = sample().encode_with(&retained);
    assert_eq!(block.len(), BLOCK);
    assert_eq!(Header::decode(&block).unwrap(), sample());
    assert_eq!(decode_retained(&block).unwrap(), retained);
    assert_eq!(
        decode_retained(&sample().encode()).unwrap(),
        Retained::default()
    );
    let mut damaged = block.clone();
    damaged[RETAINED_AT + 9] = 200;
    assert!(decode_retained(&resealed(damaged)).is_err());
}
