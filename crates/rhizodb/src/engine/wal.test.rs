use super::*;
use crate::engine::header::WalIdentity;
use crate::engine::layout::TableId;
use crate::engine::sim_fs::Crash;
use crate::engine::sim_fs::Failure;
use crate::engine::sim_fs::SimFs;
use crate::engine::wal_format::Body;
use crate::engine::wal_format::Kind;
use crate::engine::wal_format::OpRef;
use crate::engine::wal_format::batch_record;
use crate::engine::wal_format::record_prefix;
use crate::test_scale;

const BASE: &str = "graph.rhizo";
const TARGET: u64 = 4096;

fn identity() -> LogIdentity {
    LogIdentity {
        database_id: 42,
        shard_id: 0,
        salt: 0x5A17,
    }
}

fn header(checkpoint_lsn: u64, first_segment: u64) -> Header {
    Header {
        database_id: 42,
        generation: 1,
        checkpoint_lsn,
        next_extent: crate::engine::layout::ExtentId(1),
        wal: WalIdentity {
            first_segment,
            salt: identity().salt,
        },
        ..Header::default()
    }
}

fn value(lsn: u64) -> Vec<u8> {
    let length = match lsn % 7 {
        0 => 3000,
        3 => 600,
        5 => 0,
        _ => (lsn % 90) as usize,
    };
    (0..length)
        .map(|index| (lsn as usize + index) as u8)
        .collect()
}

fn record(lsn: u64) -> Vec<u8> {
    let value = value(lsn);
    let key = lsn.to_be_bytes();
    let op = OpRef::Put {
        table: TableId(0x100),
        key: &key,
        value: &value,
    };
    batch_record(lsn, lsn * 10, u128::from(lsn), [op].into_iter()).unwrap()
}

fn lsns(fs: &SimFs, header: &Header) -> Result<(Wal<SimFs>, Vec<u64>), EngineError> {
    let mut seen = Vec::new();
    let wal = Wal::recover(Arc::new(fs.clone()), BASE, header, TARGET, |record| {
        let Body::Batch(ops) = &record.body else {
            panic!("unexpected {record:?}");
        };
        assert_eq!(ops.len(), 1);
        match &ops[0] {
            crate::engine::wal_format::Op::Put {
                key, value: stored, ..
            } => {
                assert_eq!(key, &record.lsn.to_be_bytes());
                assert_eq!(stored, &value(record.lsn));
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(record.hlc, record.lsn * 10);
        seen.push(record.lsn);
        Ok(())
    })?;
    Ok((wal, seen))
}

fn fresh(fs: &SimFs) -> Wal<SimFs> {
    Wal::create(Arc::new(fs.clone()), BASE, identity(), 1, 1, 0, TARGET).unwrap()
}

#[test]
fn segment_names_sort_by_sequence_and_ignore_other_files() {
    assert_eq!(segment_name(BASE, 255), "graph.rhizo-wal-00000000000000ff");
    assert_eq!(segment_sequence(BASE, &segment_name(BASE, 255)), Some(255));
    for other in [
        "graph.rhizo",
        "graph.rhizo-wal-",
        "graph.rhizo-wal-0000000000000zz0",
        "graph.rhizo-wal-00000000000000ff0",
        "other-wal-00000000000000ff",
        "graph.rhizo-init",
    ] {
        assert_eq!(segment_sequence(BASE, other), None, "{other}");
    }
}

#[test]
fn appended_records_replay_in_order_across_segments() {
    let fs = SimFs::new();
    let mut wal = fresh(&fs);
    for lsn in 1..=60 {
        wal.append(record(lsn), lsn).unwrap();
    }
    assert!(segment_files(&fs, BASE).unwrap().len() > 5);
    assert_eq!(wal.next_lsn(), 61);
    let (mut recovered, seen) = lsns(&fs.crash(Crash::DropUnsynced), &header(0, 1)).unwrap();
    assert_eq!(seen, (1..=60).collect::<Vec<_>>());
    assert_eq!(recovered.next_lsn(), 61);
    assert_eq!(recovered.size(), wal.size());
    recovered.append(record(61), 61).unwrap();
}

#[test]
fn recovery_replays_only_the_records_after_the_checkpoint() {
    let fs = SimFs::new();
    let mut wal = fresh(&fs);
    for lsn in 1..=40 {
        wal.append(record(lsn), lsn).unwrap();
    }
    let first = wal.first_needed(25);
    assert!(first > 1);
    let checkpoint = header(25, first);
    wal.drop_before(first).unwrap();
    for crash in [Crash::DropUnsynced, Crash::KeepUnsynced] {
        let crashed = fs.crash(crash);
        let (_, seen) = lsns(&crashed, &checkpoint).unwrap();
        assert_eq!(seen, (26..=40).collect::<Vec<_>>());
        assert_eq!(segment_files(&crashed, BASE).unwrap()[0], first);
    }
}

#[test]
fn a_record_must_carry_the_next_lsn() {
    let fs = SimFs::new();
    let mut wal = fresh(&fs);
    assert!(wal.append(record(2), 0).is_err());
    wal.append(record(1), 0).unwrap();
    assert!(wal.append(record(1), 0).is_err());
    assert!(wal.append(vec![0; 10], 0).is_err());
}

#[test]
fn a_record_larger_than_a_segment_gets_one_of_its_own() {
    let fs = SimFs::new();
    let mut wal = fresh(&fs);
    let big = vec![7u8; 3 * TARGET as usize];
    let key = 1u64.to_be_bytes();
    let op = OpRef::Put {
        table: TableId(0x100),
        key: &key,
        value: &big,
    };
    wal.append(batch_record(1, 0, 1, [op].into_iter()).unwrap(), 0)
        .unwrap();
    wal.append(record(2), 0).unwrap();
    assert_eq!(segment_files(&fs, BASE).unwrap(), [1, 2]);
    let mut count = 0;
    Wal::recover(
        Arc::new(fs.crash(Crash::DropUnsynced)),
        BASE,
        &header(0, 1),
        TARGET,
        |_| {
            count += 1;
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(count, 2);
}

#[test]
fn no_segment_at_all_is_a_missing_log() {
    let fs = SimFs::new();
    assert!(matches!(
        lsns(&fs, &header(0, 1)),
        Err(EngineError::WalMissing)
    ));
    let mut wal = fresh(&fs);
    wal.append(record(1), 0).unwrap();
    assert!(matches!(
        lsns(&fs, &header(0, 2)),
        Err(EngineError::WalMissing)
    ));
}

fn filled(records: u64) -> SimFs {
    let fs = SimFs::new();
    let mut wal = fresh(&fs);
    for lsn in 1..=records {
        wal.append(record(lsn), lsn).unwrap();
    }
    fs
}

fn damage(fs: &SimFs, sequence: u64, block: u64, byte: usize) {
    let file = fs
        .open(&segment_name(BASE, sequence), wal_block().unwrap())
        .unwrap();
    let mut bytes = vec![0; WAL_BLOCK];
    file.read(block, &mut bytes).unwrap();
    bytes[byte] ^= 0x40;
    file.write(block, &bytes).unwrap();
}

#[test]
fn damage_before_the_last_segment_is_corruption() {
    for (block, byte) in [(0, 30), (1, 0), (1, 20), (1, 70), (2, 100)] {
        let fs = filled(40);
        damage(&fs, 2, block, byte);
        let error = lsns(&fs, &header(0, 1)).expect_err("damage was not noticed");
        assert!(error.is_corrupt(), "block {block} byte {byte}: {error}");
    }
}

#[test]
fn damage_in_the_last_segment_ends_the_log_there() {
    let fs = filled(40);
    let last = *segment_files(&fs, BASE).unwrap().last().unwrap();
    damage(&fs, last, 1, 20);
    let (mut wal, seen) = lsns(&fs, &header(0, 1)).unwrap();
    let survivors = seen.len() as u64;
    assert!(survivors < 40);
    assert_eq!(seen, (1..=survivors).collect::<Vec<_>>());
    wal.append(record(survivors + 1), 0).unwrap();
    let (_, again) = lsns(&fs, &header(0, 1)).unwrap();
    assert_eq!(again, (1..=survivors + 1).collect::<Vec<_>>());
}

#[test]
fn a_missing_middle_segment_is_corruption() {
    let fs = filled(40);
    fs.remove(&segment_name(BASE, 3)).unwrap();
    assert!(lsns(&fs, &header(0, 1)).err().unwrap().is_corrupt());
}

#[test]
fn a_segment_of_another_log_is_corruption() {
    let fs = filled(10);
    let other = Header {
        wal: WalIdentity {
            first_segment: 1,
            salt: 1,
        },
        ..header(0, 1)
    };
    assert!(lsns(&fs, &other).err().unwrap().is_corrupt());
    let foreign = Header {
        database_id: 43,
        ..header(0, 1)
    };
    assert!(lsns(&fs, &foreign).err().unwrap().is_corrupt());
}

#[test]
fn a_segment_renamed_into_another_place_is_corruption() {
    let fs = filled(40);
    fs.remove(&segment_name(BASE, 3)).unwrap();
    fs.rename(&segment_name(BASE, 4), &segment_name(BASE, 3))
        .unwrap();
    assert!(lsns(&fs, &header(0, 1)).err().unwrap().is_corrupt());
}

#[test]
fn a_log_that_ends_before_the_checkpoint_is_corruption() {
    let fs = filled(5);
    assert!(lsns(&fs, &header(9, 1)).err().unwrap().is_corrupt());
    assert_eq!(lsns(&fs, &header(5, 1)).unwrap().1, Vec::<u64>::new());
}

#[test]
fn a_first_segment_that_starts_after_the_checkpoint_is_corruption() {
    let fs = SimFs::new();
    let mut wal = Wal::create(Arc::new(fs.clone()), BASE, identity(), 1, 8, 0, TARGET).unwrap();
    wal.append(record(8), 0).unwrap();
    assert!(lsns(&fs, &header(3, 1)).err().unwrap().is_corrupt());
    assert_eq!(lsns(&fs, &header(7, 1)).unwrap().1, [8]);
}

#[test]
fn a_reserved_record_kind_refuses_the_log() {
    let fs = SimFs::new();
    let mut wal = fresh(&fs);
    wal.append(record(1), 0).unwrap();
    wal.append(record_prefix(2, 0, 0, 2, Kind::NoOp, 1), 0)
        .unwrap();
    wal.append(record(3), 0).unwrap();
    assert!(matches!(
        lsns(&fs, &header(0, 1)),
        Err(EngineError::Unsupported(_))
    ));
}

#[test]
fn a_last_segment_with_a_torn_header_is_an_unfinished_rollover() {
    let fs = filled(40);
    let sequences = segment_files(&fs, BASE).unwrap();
    let last = *sequences.last().unwrap();
    let file = fs
        .open(&segment_name(BASE, last), wal_block().unwrap())
        .unwrap();
    file.set_block_count(1).unwrap();
    file.write(0, &[0x33; WAL_BLOCK]).unwrap();
    let (mut wal, seen) = lsns(&fs, &header(0, 1)).unwrap();
    assert_eq!(
        segment_files(&fs, BASE).unwrap(),
        &sequences[..sequences.len() - 1]
    );
    let survivors = seen.len() as u64;
    assert_eq!(seen, (1..=survivors).collect::<Vec<_>>());
    wal.append(record(survivors + 1), 0).unwrap();
    assert_eq!(
        lsns(&fs, &header(0, 1)).unwrap().1.len() as u64,
        survivors + 1
    );
}

#[test]
fn a_torn_first_segment_header_is_corruption() {
    let fs = filled(3);
    damage(&fs, 1, 0, 40);
    assert!(lsns(&fs, &header(0, 1)).err().unwrap().is_corrupt());
}

#[test]
fn stale_segments_below_the_first_needed_are_removed() {
    let fs = SimFs::new();
    let mut wal = fresh(&fs);
    for lsn in 1..=40 {
        wal.append(record(lsn), lsn).unwrap();
    }
    let first = wal.first_needed(20);
    assert!(first > 2);
    let (_, seen) = lsns(&fs, &header(20, first)).unwrap();
    assert_eq!(seen, (21..=40).collect::<Vec<_>>());
    assert_eq!(segment_files(&fs, BASE).unwrap()[0], first);
}

#[test]
fn removing_the_whole_log_leaves_no_segment() {
    let fs = filled(40);
    let (wal, _) = lsns(&fs, &header(0, 1)).unwrap();
    wal.remove_all().unwrap();
    assert!(segment_files(&fs, BASE).unwrap().is_empty());
    assert!(
        segment_files(&fs.crash(Crash::DropUnsynced), BASE)
            .unwrap()
            .is_empty()
    );
}

const WORKLOAD: u64 = 30;

fn append_and_checkpoint_until_failure(fs: &SimFs, fail_at: Option<u64>) -> (Header, u64, bool) {
    let mut wal = fresh(fs);
    let mut durable = header(0, 1);
    if let Some(operations) = fail_at {
        fs.fail_after(operations, Failure::FromThenOn);
    }
    let mut acked = 0;
    for lsn in 1..=WORKLOAD {
        if wal.append(record(lsn), lsn).is_err() {
            return (durable, acked, true);
        }
        acked = lsn;
        if lsn % 9 == 0 {
            let checkpoint = header(lsn - 4, wal.first_needed(lsn - 4));
            durable = checkpoint;
            if wal.drop_before(checkpoint.wal.first_segment).is_err() {
                return (durable, acked, false);
            }
        }
    }
    (durable, acked, false)
}

fn crashes(seeds: u64) -> Vec<Crash> {
    let mut crashes = vec![Crash::DropUnsynced, Crash::KeepUnsynced];
    for seed in 0..seeds {
        crashes.push(Crash::Scramble { seed });
        crashes.push(Crash::Reorder { seed });
    }
    crashes
}

#[test]
fn every_failure_and_power_loss_keeps_each_acknowledged_record() {
    let total = {
        let fs = SimFs::new();
        let before = fs.operations();
        append_and_checkpoint_until_failure(&fs, None);
        fs.operations() - before
    };
    let seeds = u64::from(test_scale::cases(6));
    let mut kept_in_flight = 0;
    let mut lost_in_flight = 0;
    let mut cut_tails = 0;
    for fail_at in 0..=total {
        let fs = SimFs::new();
        let (durable, acked, in_flight) = append_and_checkpoint_until_failure(&fs, Some(fail_at));
        for crash in crashes(seeds) {
            let crashed = fs.crash(crash);
            let before = log_bytes(&crashed);
            let (mut wal, seen) = lsns(&crashed, &durable)
                .unwrap_or_else(|error| panic!("fail at {fail_at}, {crash:?}: {error}"));
            let first = durable.checkpoint_lsn + 1;
            let last = first + seen.len() as u64;
            assert_eq!(
                seen,
                (first..last).collect::<Vec<_>>(),
                "fail at {fail_at}, {crash:?}"
            );
            let recovered = last - 1;
            assert!(
                recovered == acked || (in_flight && recovered == acked + 1),
                "fail at {fail_at}, {crash:?}: acknowledged {acked}, recovered {recovered}"
            );
            if in_flight {
                if recovered > acked {
                    kept_in_flight += 1;
                } else {
                    lost_in_flight += 1;
                }
            }
            if log_bytes(&crashed) < before {
                cut_tails += 1;
            }
            assert_eq!(log_bytes(&crashed), wal.size());
            wal.append(record(last), 0).unwrap();
            wal.append(record(last + 1), 0).unwrap();
            let (_, again) = lsns(&crashed.crash(crash), &durable).unwrap();
            assert_eq!(
                again,
                (first..last + 2).collect::<Vec<_>>(),
                "fail at {fail_at}, {crash:?}"
            );
        }
    }
    assert!(total > 100, "the workload ran only {total} operations");
    assert!(kept_in_flight > 0 && lost_in_flight > 0 && cut_tails > 0);
}

fn log_bytes(fs: &SimFs) -> u64 {
    segment_files(fs, BASE)
        .unwrap()
        .into_iter()
        .map(|sequence| {
            let file = fs
                .open(&segment_name(BASE, sequence), wal_block().unwrap())
                .unwrap();
            file.block_count().unwrap() * WAL_BLOCK as u64
        })
        .sum()
}
