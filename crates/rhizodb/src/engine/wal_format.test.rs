use proptest::collection::vec;
use proptest::prelude::*;

use super::*;

const SEED: u64 = 0xABCD;

fn segment() -> SegmentHeader {
    SegmentHeader {
        minor: FORMAT_MINOR,
        database_id: 77,
        shard_id: 0,
        sequence: 4,
        first_lsn: 1001,
        first_term: 0,
        created_hlc: 123_456,
        salt: 0xFEED,
    }
}

fn ops() -> Vec<Op> {
    vec![
        Op::Put {
            table: TableId(0x100),
            key: b"k".to_vec(),
            value: b"value".to_vec(),
        },
        Op::Delete {
            table: TableId(0x101),
            key: Vec::new(),
        },
        Op::DeleteRange {
            table: TableId(0x102),
            start: b"a".to_vec(),
            end: b"z".to_vec(),
        },
        Op::Put {
            table: TableId(0x100),
            key: vec![0xFF; 300],
            value: Vec::new(),
        },
    ]
}

fn sealed(mut record: Vec<u8>) -> Vec<u8> {
    seal_record(SEED, &mut record).unwrap();
    record
}

fn batch(lsn: u64, ops: &[Op]) -> Vec<u8> {
    sealed(batch_record(lsn, 9, u128::from(lsn), ops.iter().map(Op::borrowed)).unwrap())
}

#[test]
fn a_segment_header_round_trips_in_one_block() {
    let block = segment().encode();
    assert_eq!(block.len(), WAL_BLOCK);
    assert_eq!(&block[..8], b"RZWALSEG");
    assert_eq!(SegmentHeader::decode(&block).unwrap(), segment());
}

#[test]
fn every_flipped_bit_of_a_segment_header_is_caught() {
    let block = segment().encode();
    for byte in 0..WAL_BLOCK {
        for bit in 0..8 {
            let mut damaged = block.clone();
            damaged[byte] ^= 1 << bit;
            assert!(
                SegmentHeader::decode(&damaged).is_err(),
                "byte {byte} bit {bit}"
            );
        }
    }
}

#[test]
fn a_newer_segment_format_is_unsupported() {
    let mut header = segment().encode();
    header[8] = 2;
    let sum = checksum(0, &[&header[..SEGMENT_CHECKSUM_AT]]);
    header[SEGMENT_CHECKSUM_AT..].copy_from_slice(&sum.to_le_bytes());
    assert!(matches!(
        SegmentHeader::decode(&header),
        Err(EngineError::Unsupported(_))
    ));
}

#[test]
fn record_seeds_differ_by_segment_and_salt() {
    let header = segment();
    let next = SegmentHeader {
        sequence: 5,
        ..header
    };
    let salted = SegmentHeader { salt: 1, ..header };
    assert_ne!(header.record_seed(), next.record_seed());
    assert_ne!(header.record_seed(), salted.record_seed());
}

#[test]
fn a_batch_record_round_trips() {
    let record = batch(1001, &ops());
    let (decoded, length) = decode_record(SEED, &record).unwrap();
    assert_eq!(length, record.len());
    assert_eq!(
        decoded,
        Record {
            lsn: 1001,
            term: 0,
            hlc: 9,
            txn: 1001,
            body: Body::Batch(ops()),
        }
    );
}

#[test]
fn a_checkpoint_record_round_trips() {
    let record = sealed(checkpoint_record(5, 6, 4, 3));
    let (decoded, _) = decode_record(SEED, &record).unwrap();
    assert_eq!(
        decoded.body,
        Body::Checkpoint {
            lsn: 4,
            generation: 3
        }
    );
}

#[test]
fn an_ingest_record_round_trips_and_refuses_a_body() {
    let record = sealed(ingest_record(7, 8, 7));
    let (decoded, length) = decode_record(SEED, &record).unwrap();
    assert_eq!(length, record.len());
    assert_eq!(
        decoded,
        Record {
            lsn: 7,
            term: 0,
            hlc: 8,
            txn: 7,
            body: Body::Ingest,
        }
    );
    let mut longer = ingest_record(7, 8, 7);
    longer.push(0);
    assert!(decode_record(SEED, &sealed(longer)).is_err());
    let mut newer = record_prefix(7, 0, 8, 7, Kind::Ingest, 2);
    newer.truncate(RECORD_HEADER);
    assert!(matches!(
        decode_record(SEED, &resealed(newer)),
        Err(NotRecord::Refused(EngineError::Unsupported(_)))
    ));
}

#[test]
fn every_flipped_bit_of_a_record_makes_it_invalid() {
    let record = batch(3, &ops());
    for byte in 0..record.len() {
        for bit in 0..8 {
            let mut damaged = record.clone();
            damaged[byte] ^= 1 << bit;
            let mut padded = damaged.clone();
            padded.resize(record.len() + 64, 0);
            assert!(
                matches!(
                    decode_record(SEED, &padded),
                    Err(NotRecord::Invalid(_) | NotRecord::End)
                ),
                "byte {byte} bit {bit}"
            );
        }
    }
}

#[test]
fn a_record_from_another_segment_is_invalid() {
    let record = batch(3, &ops());
    assert!(matches!(
        decode_record(SEED + 1, &record),
        Err(NotRecord::Invalid(_))
    ));
}

#[test]
fn every_cut_of_a_record_is_invalid_or_the_end() {
    let record = batch(3, &ops());
    for cut in 0..record.len() {
        let outcome = decode_record(SEED, &record[..cut]);
        assert!(
            matches!(outcome, Err(NotRecord::Invalid(_) | NotRecord::End)),
            "cut at {cut}"
        );
    }
}

#[test]
fn zeros_are_the_end_of_the_log() {
    assert!(matches!(
        decode_record(SEED, &[0; 512]),
        Err(NotRecord::End)
    ));
    assert!(matches!(decode_record(SEED, &[]), Err(NotRecord::End)));
    assert!(matches!(decode_record(SEED, &[1, 0]), Err(NotRecord::End)));
}

fn resealed(prefix: Vec<u8>) -> Vec<u8> {
    let mut record = prefix;
    seal_record(SEED, &mut record).unwrap();
    record
}

#[test]
fn reserved_kinds_and_unknown_formats_are_refused_not_skipped() {
    for kind in Kind::ALL {
        if matches!(kind, Kind::Batch | Kind::Ingest | Kind::Checkpoint) {
            continue;
        }
        let record = resealed(record_prefix(1, 0, 0, 1, kind, 1));
        assert!(
            matches!(
                decode_record(SEED, &record),
                Err(NotRecord::Refused(EngineError::Unsupported(_)))
            ),
            "{kind:?}"
        );
    }
    let mut unknown = record_prefix(1, 0, 0, 1, Kind::Batch, 1);
    unknown[52] = 99;
    assert!(matches!(
        decode_record(SEED, &resealed(unknown)),
        Err(NotRecord::Refused(EngineError::Unsupported(_)))
    ));
    let mut newer = record_prefix(1, 0, 0, 1, Kind::Batch, 2);
    put_u32(&mut newer, 0);
    assert!(matches!(
        decode_record(SEED, &resealed(newer)),
        Err(NotRecord::Refused(EngineError::Unsupported(_)))
    ));
}

#[test]
fn unknown_and_patch_operations_are_refused() {
    for tag in [0u8, PATCH, 5, 0xFF] {
        let mut record = record_prefix(1, 0, 0, 1, Kind::Batch, BATCH_FORMAT);
        put_u32(&mut record, 1);
        record.push(tag);
        put_u16(&mut record, 0x100);
        put_bytes(&mut record, b"k").unwrap();
        assert!(matches!(
            decode_record(SEED, &resealed(record)),
            Err(NotRecord::Refused(EngineError::Unsupported(_)))
        ));
    }
}

#[test]
fn a_sealed_but_malformed_body_is_corrupt() {
    let mut short = record_prefix(1, 0, 0, 1, Kind::Batch, BATCH_FORMAT);
    put_u32(&mut short, 3);
    let mut trailing = record_prefix(1, 0, 0, 1, Kind::Batch, BATCH_FORMAT);
    put_u32(&mut trailing, 0);
    trailing.push(7);
    let mut zero_lsn = record_prefix(0, 0, 0, 1, Kind::Batch, BATCH_FORMAT);
    put_u32(&mut zero_lsn, 0);
    for record in [short, trailing, zero_lsn] {
        match decode_record(SEED, &resealed(record)) {
            Err(NotRecord::Refused(error)) => assert!(error.is_corrupt(), "{error}"),
            other => panic!("expected corruption, got {other:?}"),
        }
    }
}

#[test]
fn lengths_outside_the_limits_are_invalid() {
    let mut bytes = vec![0; 64];
    bytes[..4].copy_from_slice(&10u32.to_le_bytes());
    assert!(matches!(record_length(&bytes), Err(NotRecord::Invalid(_))));
    bytes[..4].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(matches!(record_length(&bytes), Err(NotRecord::Invalid(_))));
    let mut tiny = vec![0; 10];
    assert!(seal_record(SEED, &mut tiny).is_err());
}

#[test]
fn padding_rounds_up_to_whole_blocks() {
    assert_eq!(padded(1), WAL_BLOCK);
    assert_eq!(padded(WAL_BLOCK), WAL_BLOCK);
    assert_eq!(padded(WAL_BLOCK + 1), 2 * WAL_BLOCK);
}

fn op_strategy() -> impl Strategy<Value = Op> {
    let table = (0u16..4).prop_map(|id| TableId(0x100 + id));
    let bytes = || vec(any::<u8>(), 0..40);
    prop_oneof![
        (table.clone(), bytes(), vec(any::<u8>(), 0..3000))
            .prop_map(|(table, key, value)| Op::Put { table, key, value }),
        (table.clone(), bytes()).prop_map(|(table, key)| Op::Delete { table, key }),
        (table, bytes(), bytes()).prop_map(|(table, start, end)| Op::DeleteRange {
            table,
            start,
            end
        }),
    ]
}

proptest! {
    #[test]
    fn any_batch_round_trips(ops in vec(op_strategy(), 0..20), lsn in 1u64.., hlc in any::<u64>()) {
        let record = sealed(batch_record(lsn, hlc, 5, ops.iter().map(Op::borrowed)).unwrap());
        let (decoded, length) = decode_record(SEED, &record).unwrap();
        prop_assert_eq!(length, record.len());
        prop_assert_eq!(decoded, Record { lsn, term: 0, hlc, txn: 5, body: Body::Batch(ops) });
    }

    #[test]
    fn any_bytes_decode_or_fail_without_panicking(bytes in vec(any::<u8>(), 0..600), seal in any::<bool>()) {
        let mut bytes = bytes;
        if seal && bytes.len() >= RECORD_HEADER {
            seal_record(SEED, &mut bytes).unwrap();
        }
        let _ = decode_record(SEED, &bytes);
        let _ = SegmentHeader::decode(&bytes);
    }

    #[test]
    fn any_sealed_record_body_decodes_or_is_refused(body in vec(any::<u8>(), 0..300), kind in 0u8..30, format in 0u8..3) {
        let mut record = record_prefix(1, 0, 0, 1, Kind::Batch, format);
        record[52] = kind;
        record.extend_from_slice(&body);
        seal_record(SEED, &mut record).unwrap();
        prop_assert!(!matches!(decode_record(SEED, &record), Err(NotRecord::Invalid(_) | NotRecord::End)));
    }
}
