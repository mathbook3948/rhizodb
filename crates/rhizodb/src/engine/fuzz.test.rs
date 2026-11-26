use proptest::collection::vec;
use proptest::prelude::*;

use super::*;
use crate::engine::header::FORMAT_MINOR;
use crate::engine::layout::ExtentId;
use crate::engine::layout::TableId;
use crate::engine::page::PageKind;
use crate::engine::wal_format::Op;
use crate::engine::wal_format::batch_record;
use crate::engine::wal_format::checkpoint_record;

#[test]
fn valid_encodings_of_every_format_round_trip() {
    let header = Header {
        minor: FORMAT_MINOR,
        generation: 3,
        next_extent: ExtentId(1),
        ..Header::default()
    };
    assert_eq!(
        decode_formats(&[[0].as_slice(), &header.encode()].concat()),
        Ok(())
    );
    let segment = SegmentHeader {
        first_lsn: 1,
        ..SegmentHeader::default()
    };
    assert_eq!(
        decode_formats(&[[1].as_slice(), &segment.encode()].concat()),
        Ok(())
    );
    let at = BlockRef::new(ExtentId(2), 5).unwrap();
    let mut page = vec![0; BLOCK];
    PageHeader {
        kind: PageKind::Leaf,
        format: 1,
        table: TableId(0x100),
        level: 0,
        flags: 0,
        cells: 0,
        free: 0,
        at,
        lsn: 1,
        generation: 1,
    }
    .seal(0, &mut page)
    .unwrap();
    assert_eq!(decode_formats(&[[2].as_slice(), &page].concat()), Ok(()));
    let ops = [Op::DeleteRange {
        table: TableId(0x100),
        start: vec![1],
        end: vec![2],
    }];
    let mut record = batch_record(4, 5, 77, ops.iter().map(Op::borrowed)).unwrap();
    record[20..28].copy_from_slice(&9u64.to_le_bytes());
    seal_record(0, &mut record).unwrap();
    assert_eq!(decode_formats(&[[3].as_slice(), &record].concat()), Ok(()));
    let mut checkpoint = checkpoint_record(4, 5, 3, 2);
    seal_record(0, &mut checkpoint).unwrap();
    assert_eq!(
        decode_formats(&[[3].as_slice(), &checkpoint].concat()),
        Ok(())
    );
}

#[test]
fn the_undamaged_file_reads_back() {
    assert_eq!(open_damaged_file(&[]), Ok(()));
}

#[test]
fn the_undamaged_stream_installs_and_reads_back() {
    assert_eq!(install_damaged_stream(&[]), Ok(()));
}

#[test]
fn a_stream_whose_range_count_or_history_length_is_forged_fails_to_install() {
    for at in 48..64 {
        assert_eq!(install_damaged_stream(&[0, 0, at, 0xff]), Ok(()));
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(crate::test_scale::cases(256)))]

    #[test]
    #[ignore = "left out of mise run check, run by mise run random"]
    fn a_damaged_file_never_panics_or_reads_differently(damage in vec(any::<u8>(), 4..64)) {
        prop_assert_eq!(open_damaged_file(&damage), Ok(()));
    }

    #[test]
    #[ignore = "left out of mise run check, run by mise run random"]
    fn a_damaged_stream_never_panics_or_installs_differently(damage in vec(any::<u8>(), 4..64)) {
        prop_assert_eq!(install_damaged_stream(&damage), Ok(()));
    }

    #[test]
    #[ignore = "left out of mise run check, run by mise run random"]
    fn arbitrary_bytes_never_break_a_decoder(choice in 0u8..10, rest in vec(any::<u8>(), 0..5000)) {
        let input = [[choice].as_slice(), &rest].concat();
        prop_assert_eq!(decode_formats(&input), Ok(()));
    }
}
