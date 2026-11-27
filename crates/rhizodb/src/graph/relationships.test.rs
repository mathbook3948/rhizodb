use std::collections::BTreeMap;

use super::*;
use proptest::prelude::*;

const KEY: [u8; 8] = 7u64.to_be_bytes();

fn edits() -> impl Strategy<Value = Vec<(usize, Option<Vec<u8>>)>> {
    prop::collection::vec(
        (
            0..CHUNK_SLOTS,
            prop::option::weighted(0.8, prop::collection::vec(any::<u8>(), 0..200)),
        ),
        0..120,
    )
}

proptest! {
    #[test]
    #[ignore = "left out of mise run check, run by mise run random"]
    fn a_chunk_reads_back_what_its_edits_left(edits in edits()) {
        let mut edit = ChunkEdit::default();
        let mut model = BTreeMap::new();
        for (slot, record) in edits {
            match &record {
                Some(record) => model.insert(slot, record.clone()),
                None => model.remove(&slot),
            };
            edit.set(slot, record.as_deref());
        }
        prop_assert_eq!(edit.bytes(), model.values().map(Vec::len).sum::<usize>());
        let Some(stored) = edit.encode(&KEY) else {
            prop_assert!(model.is_empty());
            return Ok(());
        };
        let chunk = ChunkRef::parse(&KEY, &stored).unwrap();
        for slot in 0..CHUNK_SLOTS {
            prop_assert_eq!(chunk.get(slot).unwrap(), model.get(&slot).map(Vec::as_slice));
        }
        let entries: Vec<(usize, Vec<u8>)> = chunk
            .entries()
            .map(|entry| entry.map(|(slot, record)| (slot, record.to_vec())))
            .collect::<Result<_, _>>()
            .unwrap();
        prop_assert_eq!(entries, model.clone().into_iter().collect::<Vec<_>>());
        let loaded = ChunkEdit::load(&KEY, &stored).unwrap();
        prop_assert_eq!(loaded.encode(&KEY), Some(stored.clone()));
        prop_assert_eq!(seal(&KEY, &unsealed(&stored)).unwrap(), Some(stored.clone()));
        prop_assert_eq!(seal(&KEY, &stored).unwrap(), None);
    }

    #[test]
    #[ignore = "left out of mise run check, run by mise run random"]
    fn a_damaged_chunk_fails_to_read(
        edits in edits(),
        cut in any::<prop::sample::Index>(),
        flip in any::<prop::sample::Index>(),
    ) {
        let mut edit = ChunkEdit::default();
        for (slot, record) in edits {
            edit.set(slot, record.as_deref());
        }
        let Some(mut stored) = edit.encode(&KEY) else {
            return Ok(());
        };
        let at = flip.index(stored.len());
        stored[at] ^= 0x5a;
        stored.truncate(cut.index(stored.len() + 1));
        prop_assert!(ChunkRef::parse(&KEY, &stored).is_err());
        prop_assert!(ChunkEdit::load(&KEY, &stored).is_err());
        prop_assert!(seal(&KEY, &stored).is_err());
    }

    #[test]
    #[ignore = "left out of mise run check, run by mise run random"]
    fn a_damaged_unsealed_chunk_fails_to_seal_or_seals_its_framed_records(
        edits in edits(),
        cut in any::<prop::sample::Index>(),
        flip in any::<prop::sample::Index>(),
    ) {
        let mut edit = ChunkEdit::default();
        for (slot, record) in edits {
            edit.set(slot, record.as_deref());
        }
        let Some(stored) = edit.encode(&KEY) else {
            return Ok(());
        };
        let mut old = unsealed(&stored);
        let at = flip.index(old.len());
        old[at] ^= 0x5a;
        old.truncate(cut.index(old.len() + 1));
        if let Ok(Some(sealed)) = seal(&KEY, &old) {
            let chunk = ChunkRef::parse(&KEY, &sealed).unwrap();
            prop_assert!(chunk.entries().all(|entry| entry.is_ok()));
        }
    }
}

#[test]
fn a_relationship_id_splits_into_its_chunk_and_slot() {
    for id in [0, 1, 63, 64, 65, 4095, u64::MAX] {
        let (chunk, slot) = locate(RelationshipId(id));
        assert!(slot < CHUNK_SLOTS);
        assert_eq!(relationship_at(chunk, slot), RelationshipId(id));
    }
    assert_eq!(locate(RelationshipId(66)), (2, 2));
    assert_eq!(parse_key(&chunk_key(7)).unwrap(), 7);
}

#[test]
fn an_unsealed_chunk_with_trailing_bytes_fails_to_seal() {
    let mut edit = ChunkEdit::default();
    edit.set(3, Some(&[1, 2]));
    let mut stored = unsealed(&edit.encode(&KEY).unwrap());
    stored.push(0);
    assert!(seal(&KEY, &stored).is_err());
}

#[test]
fn an_empty_or_unknown_chunk_fails_to_parse() {
    let sealed = |bytes: &[u8]| seal(&KEY, bytes).unwrap().unwrap();
    assert!(ChunkRef::parse(&KEY, &[]).is_err());
    assert!(ChunkRef::parse(&KEY, &[2, 0, 0, 0]).is_err());
    assert!(seal(&KEY, &[1, 0, 0, 0, 0]).is_err());
    assert!(seal(&KEY, &[3, 1, 0, 0, 0, 0]).is_err());
    assert!(ChunkRef::parse(&KEY, &[1, 1, 0, 0, 0, 0]).is_err());
    assert!(ChunkRef::parse(&KEY, &sealed(&[1, 1, 0, 0, 0, 0])).is_ok());
}

#[test]
fn every_flipped_bit_of_a_chunk_fails_its_checksum() {
    let mut edit = ChunkEdit::default();
    edit.set(0, Some(&[]));
    edit.set(3, Some(&[1, 2, 3]));
    edit.set(31, Some(&[9; 40]));
    let stored = edit.encode(&KEY).unwrap();
    for at in 0..stored.len() {
        for bit in 0..8 {
            let mut damaged = stored.clone();
            damaged[at] ^= 1 << bit;
            assert!(
                ChunkRef::parse(&KEY, &damaged).is_err(),
                "byte {at} bit {bit}"
            );
            assert!(
                ChunkEdit::load(&KEY, &damaged).is_err(),
                "byte {at} bit {bit}"
            );
        }
    }
    for cut in 0..stored.len() {
        assert!(
            ChunkRef::parse(&KEY, &stored[..cut]).is_err(),
            "cut at {cut}"
        );
    }
}

#[test]
fn a_chunk_under_another_chunks_key_fails_its_checksum() {
    let mut edit = ChunkEdit::default();
    edit.set(5, Some(&[4, 2]));
    let stored = edit.encode(&KEY).unwrap();
    assert!(ChunkRef::parse(&KEY, &stored).is_ok());
    assert!(ChunkRef::parse(&8u64.to_be_bytes(), &stored).is_err());
}
