use std::collections::BTreeMap;

use super::*;
use proptest::prelude::*;

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
        let Some(stored) = edit.encode() else {
            prop_assert!(model.is_empty());
            return Ok(());
        };
        let chunk = ChunkRef::parse(&stored).unwrap();
        for slot in 0..CHUNK_SLOTS {
            prop_assert_eq!(chunk.get(slot).unwrap(), model.get(&slot).map(Vec::as_slice));
        }
        let entries: Vec<(usize, Vec<u8>)> = chunk
            .entries()
            .map(|entry| entry.map(|(slot, record)| (slot, record.to_vec())))
            .collect::<Result<_, _>>()
            .unwrap();
        prop_assert_eq!(entries, model.clone().into_iter().collect::<Vec<_>>());
        let loaded = ChunkEdit::load(&stored).unwrap();
        prop_assert_eq!(loaded.encode(), Some(stored));
    }

    #[test]
    fn a_damaged_chunk_fails_to_read_or_reads_some_records(
        edits in edits(),
        cut in any::<prop::sample::Index>(),
        flip in any::<prop::sample::Index>(),
    ) {
        let mut edit = ChunkEdit::default();
        for (slot, record) in edits {
            edit.set(slot, record.as_deref());
        }
        let Some(mut stored) = edit.encode() else {
            return Ok(());
        };
        let at = flip.index(stored.len());
        stored[at] ^= 0x5a;
        stored.truncate(cut.index(stored.len() + 1));
        if let Ok(chunk) = ChunkRef::parse(&stored) {
            for slot in 0..CHUNK_SLOTS {
                let _ = chunk.get(slot);
            }
            let _ = chunk.entries().count();
            let _ = ChunkEdit::load(&stored);
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
fn a_chunk_with_trailing_bytes_fails_when_listed() {
    let mut edit = ChunkEdit::default();
    edit.set(3, Some(&[1, 2]));
    let mut stored = edit.encode().unwrap();
    stored.push(0);
    let chunk = ChunkRef::parse(&stored).unwrap();
    assert!(chunk.entries().any(|entry| entry.is_err()));
}

#[test]
fn an_empty_or_unknown_chunk_fails_to_parse() {
    assert!(ChunkRef::parse(&[]).is_err());
    assert!(ChunkRef::parse(&[1, 0, 0, 0, 0]).is_err());
    assert!(ChunkRef::parse(&[2, 1, 0, 0, 0, 0]).is_err());
    assert!(ChunkRef::parse(&[1, 1, 0, 0, 0, 0]).is_ok());
}
