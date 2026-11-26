use super::*;
use crate::test_scale;
use proptest::prelude::*;

const GROUP: u64 = 3;

fn labels_of(set: LabelSetId) -> Result<Arc<[Token<Label>]>, GraphError> {
    let mut labels = vec![Token::new(set.0 % 5), Token::new(5 + set.0 % 7)];
    labels.sort();
    labels.dedup();
    Ok(labels.into())
}

fn edit_of(live: &BTreeMap<usize, u32>) -> MetaEdit {
    let mut edit = MetaEdit::default();
    for (slot, set) in live {
        edit.put(*slot, LabelSetId(*set));
    }
    edit
}

fn live_slots(sets: std::ops::Range<u32>) -> impl Strategy<Value = BTreeMap<usize, u32>> {
    prop::collection::btree_map(0..SLOTS, sets, 0..300)
}

fn chunk_of(live: &BTreeMap<usize, u32>) -> Vec<u8> {
    edit_of(live).encode(GROUP, labels_of).unwrap().unwrap()
}

fn slot_state() -> impl Strategy<Value = Slot> {
    prop_oneof![Just(None), (0u32..6).prop_map(|set| Some(LabelSetId(set)))]
}

fn delta() -> impl Strategy<Value = Delta> {
    (0..SLOTS, slot_state(), slot_state()).prop_map(|(slot, before, after)| Delta {
        slot,
        before,
        after,
    })
}

proptest! {
    #[test]
    #[ignore = "left out of mise run check, run by mise run random"]
    fn a_chunk_reads_back_each_slot_and_label_count_it_was_encoded_with(
        live in prop_oneof![live_slots(0..1), live_slots(1..3), live_slots(0..400)],
        label in (0u32..13).prop_map(Token::new),
    ) {
        let edit = edit_of(&live);
        let Some(chunk) = edit.encode(GROUP, labels_of).unwrap() else {
            prop_assert!(live.is_empty());
            return Ok(());
        };
        let stored = MetaBytes::parse(GROUP, &chunk).unwrap();
        for slot in 0..SLOTS {
            prop_assert_eq!(
                stored.label_set(slot).unwrap(),
                live.get(&slot).map(|set| LabelSetId(*set))
            );
        }
        let carrying = live
            .values()
            .filter(|set| labels_of(LabelSetId(**set)).unwrap().contains(&label))
            .count() as u64;
        prop_assert_eq!(stored.head().label_count(label), carrying);
        prop_assert_eq!(MetaHead::parse(GROUP, &chunk).unwrap().label_count(label), carrying);
        prop_assert_eq!(stored.to_edit().unwrap(), edit);
    }

    #[test]
    #[ignore = "left out of mise run check, run by mise run random"]
    fn a_cut_or_padded_chunk_is_rejected(
        live in live_slots(0..400),
        cut in 1usize..64,
    ) {
        prop_assume!(!live.is_empty());
        let chunk = chunk_of(&live);
        let cut = &chunk[..chunk.len() - cut.min(chunk.len())];
        prop_assert!(MetaBytes::parse(GROUP, cut).is_err());
        prop_assert!(MetaHead::parse(GROUP, cut).is_err());
        let mut padded = chunk.clone();
        padded.push(0);
        prop_assert!(MetaBytes::parse(GROUP, &padded).is_err());
        prop_assert!(MetaHead::parse(GROUP, &padded).is_err());
    }

    #[test]
    #[ignore = "left out of mise run check, run by mise run random"]
    fn a_chunk_with_any_byte_changed_is_rejected_and_its_head_check_covers_its_head(
        live in prop_oneof![live_slots(0..1), live_slots(1..3), live_slots(0..400)],
        at in any::<prop::sample::Index>(),
        flip in 1u8..=255,
    ) {
        prop_assume!(!live.is_empty());
        let mut chunk = chunk_of(&live);
        let body = Layout::of(&chunk).unwrap().body;
        let at = at.index(chunk.len());
        chunk[at] ^= flip;
        prop_assert!(MetaBytes::parse(GROUP, &chunk).is_err());
        if at < body {
            prop_assert!(MetaHead::parse(GROUP, &chunk).is_err());
        }
    }

    #[test]
    #[ignore = "left out of mise run check, run by mise run random"]
    fn a_chunk_is_rejected_under_the_key_of_another_group(
        live in live_slots(0..400),
        other in any::<u64>(),
    ) {
        prop_assume!(!live.is_empty() && other != GROUP);
        let chunk = chunk_of(&live);
        prop_assert!(MetaBytes::parse(other, &chunk).is_err());
        prop_assert!(MetaHead::parse(other, &chunk).is_err());
    }

    #[test]
    #[ignore = "left out of mise run check, run by mise run random"]
    fn a_delta_reads_back_and_rejects_any_changed_byte_or_another_key(
        delta in delta(),
        at in 0..DELTA,
        flip in 1u8..=255,
        other in 0..SLOTS,
    ) {
        let key = delta_key(GROUP, delta.slot);
        let mut bytes = delta.encode(GROUP);
        prop_assert_eq!(Delta::parse(&key, &bytes).unwrap(), delta);
        if other != delta.slot {
            prop_assert!(Delta::parse(&delta_key(GROUP, other), &bytes).is_err());
        }
        prop_assert!(Delta::parse(&delta_key(GROUP + 1, delta.slot), &bytes).is_err());
        prop_assert!(Delta::parse(&meta_key(GROUP), &bytes).is_err());
        prop_assert!(Delta::parse(&key, &bytes[..DELTA - 1]).is_err());
        bytes[at] ^= flip;
        prop_assert!(Delta::parse(&key, &bytes).is_err());
    }

    #[test]
    #[ignore = "left out of mise run check, run by mise run random"]
    fn a_group_stored_as_a_chunk_and_deltas_reads_back_as_edited_and_keeps_few_deltas(
        initial in live_slots(0..4),
        transactions in prop::collection::vec(
            prop::collection::vec(
                (prop_oneof![0..80usize, 0..SLOTS], slot_state()),
                0..test_scale::cases(40) as usize,
            ),
            1..12,
        ),
    ) {
        let mut chunk = (!initial.is_empty()).then(|| edit_of(&initial));
        let mut deltas: BTreeMap<usize, Delta> = BTreeMap::new();
        let mut model = edit_of(&initial);
        for changes in transactions {
            let mut edit =
                GroupEdit::load(chunk.as_ref().map(sealed), deltas.values().copied().collect())
                    .unwrap();
            prop_assert_eq!(edit.view().unwrap(), &model);
            for (slot, state) in changes {
                edit.set(slot, state).unwrap();
                model.set(slot, state);
            }
            prop_assert_eq!(edit.view().unwrap(), &model);
            match edit.flush().unwrap() {
                Flush::Rewrite { view, stale } => {
                    let view = *view;
                    prop_assert_eq!(&view, &model);
                    prop_assert_eq!(stale, deltas.keys().copied().collect::<Vec<_>>());
                    chunk = (!view.is_empty()).then_some(view);
                    deltas.clear();
                }
                Flush::Deltas { put, delete } => {
                    prop_assert!(chunk.is_some());
                    for slot in delete {
                        prop_assert!(deltas.remove(&slot).is_some());
                    }
                    for delta in put {
                        prop_assert_ne!(delta.before, delta.after);
                        deltas.insert(delta.slot, delta);
                    }
                }
            }
            prop_assert!(deltas.len() <= MAX_DELTAS);
            let mut stored = chunk.clone().unwrap_or_default();
            for delta in deltas.values() {
                prop_assert_eq!(
                    chunk.as_ref().and_then(|chunk| chunk.label_set(delta.slot)),
                    delta.before
                );
                stored.set(delta.slot, delta.after);
            }
            prop_assert_eq!(&stored, &model);
        }
    }
}

#[test]
fn a_group_of_one_label_set_stores_no_codes() {
    let edit = edit_of(&(0..SLOTS).map(|slot| (slot, 3)).collect());
    let chunk = edit.encode(GROUP, labels_of).unwrap().unwrap();
    assert!(chunk.len() < 600);
}

#[test]
fn a_group_without_live_nodes_has_no_chunk() {
    let mut edit = edit_of(&BTreeMap::from([(7, 2)]));
    edit.remove(7);
    assert_eq!(edit.encode(GROUP, labels_of).unwrap(), None);
}

#[test]
fn a_node_id_maps_to_its_group_and_slot_and_back() {
    for id in [0, 1, 4095, 4096, 4097, u64::MAX] {
        let (group, slot) = locate(NodeId(id));
        assert_eq!(node_at(group, slot), NodeId(id));
    }
    assert_eq!(locate(NodeId(4097)), (1, 1));
}

#[test]
fn node_meta_keys_order_by_group_then_chunk_then_slot() {
    assert!(meta_key(1) < delta_key(1, 0));
    assert!(delta_key(1, 0) < delta_key(1, 1));
    assert!(delta_key(1, 255) < delta_key(1, 256));
    assert!(delta_key(1, SLOTS - 1) < meta_key(2));
    assert!(delta_key(255, SLOTS - 1) < meta_key(256));
    assert!(meta_key(9).starts_with(&group_prefix(9)));
    assert!(delta_key(9, 4095).starts_with(&group_prefix(9)));
    assert_eq!(parse_key(&meta_key(9)).unwrap(), Some((9, Part::Chunk)));
    assert_eq!(
        parse_key(&delta_key(9, 4095)).unwrap(),
        Some((9, Part::Delta(4095)))
    );
}

#[test]
fn a_node_meta_key_of_another_kind_length_or_part_is_not_read_as_node_meta() {
    let mut other_kind = meta_key(9);
    other_kind[8] = 1;
    assert_eq!(parse_key(&other_kind).unwrap(), None);
    assert!(parse_key(&meta_key(9)[..12]).is_err());
    assert!(parse_key(&key(9, SLOTS as u32 + 1)).is_err());
    assert!(parse_key(&key(9, u32::MAX)).is_err());
}

#[test]
fn a_delta_has_fourteen_bytes_and_a_slot_state_holds_no_stray_bits() {
    let delta = Delta {
        slot: 1,
        before: None,
        after: Some(LabelSetId(u32::MAX)),
    };
    assert_eq!(delta.encode(GROUP).len(), 14);
    assert!(parse_state(&[0, 0, 0, 0, 1]).is_err());
    assert!(parse_state(&[2, 0, 0, 0, 0]).is_err());
    assert_eq!(
        parse_state(&[1, 0, 0, 0, 0]).unwrap(),
        Some(LabelSetId::EMPTY)
    );
}

fn sealed(edit: &MetaEdit) -> Box<[u8]> {
    edit.encode(GROUP, labels_of).unwrap().unwrap().into()
}

fn stored_edit() -> MetaEdit {
    edit_of(&(0..100).map(|slot| (slot, 1)).collect())
}

#[test]
fn a_group_without_a_chunk_is_written_as_a_chunk() {
    let mut edit = GroupEdit::load(None, Vec::new()).unwrap();
    edit.set(5, Some(LabelSetId(1))).unwrap();
    assert!(matches!(edit.flush().unwrap(), Flush::Rewrite { stale, .. } if stale.is_empty()));
}

#[test]
fn a_few_changes_to_a_stored_chunk_are_written_as_deltas() {
    let mut edit = GroupEdit::load(Some(sealed(&stored_edit())), Vec::new()).unwrap();
    edit.set(5, None).unwrap();
    edit.set(200, Some(LabelSetId(2))).unwrap();
    edit.set(7, Some(LabelSetId(1))).unwrap();
    let Flush::Deltas { put, delete } = edit.flush().unwrap() else {
        panic!("expected deltas");
    };
    assert_eq!(
        put,
        vec![
            Delta {
                slot: 5,
                before: Some(LabelSetId(1)),
                after: None,
            },
            Delta {
                slot: 200,
                before: None,
                after: Some(LabelSetId(2)),
            },
        ]
    );
    assert!(delete.is_empty());
}

#[test]
fn a_slot_changed_back_to_its_chunk_state_drops_its_delta() {
    let stored = Delta {
        slot: 5,
        before: Some(LabelSetId(1)),
        after: None,
    };
    let mut edit = GroupEdit::load(Some(sealed(&stored_edit())), vec![stored]).unwrap();
    assert_eq!(edit.label_set(5).unwrap(), None);
    edit.set(5, Some(LabelSetId(1))).unwrap();
    let Flush::Deltas { put, delete } = edit.flush().unwrap() else {
        panic!("expected deltas");
    };
    assert!(put.is_empty());
    assert_eq!(delete, vec![5]);
}

#[test]
fn a_group_past_the_delta_limit_rewrites_its_chunk_and_drops_its_deltas() {
    let stored: Vec<Delta> = (0..MAX_DELTAS)
        .map(|slot| Delta {
            slot,
            before: Some(LabelSetId(1)),
            after: Some(LabelSetId(2)),
        })
        .collect();
    let mut edit = GroupEdit::load(Some(sealed(&stored_edit())), stored.clone()).unwrap();
    edit.set(0, Some(LabelSetId(3))).unwrap();
    assert!(matches!(edit.flush().unwrap(), Flush::Deltas { .. }));
    let mut edit = GroupEdit::load(Some(sealed(&stored_edit())), stored).unwrap();
    edit.set(MAX_DELTAS, Some(LabelSetId(3))).unwrap();
    let Flush::Rewrite { view, stale } = edit.flush().unwrap() else {
        panic!("expected a rewrite");
    };
    assert_eq!(stale, (0..MAX_DELTAS).collect::<Vec<_>>());
    assert_eq!(view.label_set(0), Some(LabelSetId(2)));
    assert_eq!(view.label_set(MAX_DELTAS), Some(LabelSetId(3)));
    assert_eq!(view.label_set(MAX_DELTAS + 1), Some(LabelSetId(1)));
}

#[test]
fn a_group_emptied_through_deltas_deletes_its_chunk() {
    let mut edit = GroupEdit::load(
        Some(sealed(&edit_of(&BTreeMap::from([(3, 1)])))),
        Vec::new(),
    )
    .unwrap();
    edit.set(3, None).unwrap();
    let Flush::Rewrite { view, .. } = edit.flush().unwrap() else {
        panic!("expected a rewrite");
    };
    assert_eq!(view.encode(GROUP, labels_of).unwrap(), None);
}

#[test]
fn deltas_that_do_not_amend_a_chunk_are_corrupt() {
    let delta = Delta {
        slot: 500,
        before: Some(LabelSetId(1)),
        after: None,
    };
    assert!(GroupEdit::load(None, vec![delta]).is_err());
    assert!(GroupEdit::load(Some(sealed(&stored_edit())), vec![delta]).is_err());
}

#[test]
fn a_resealed_chunk_with_a_wrong_label_count_passes_its_checksums() {
    let mut chunk = chunk_of(&BTreeMap::from([(3, 1), (9, 2)]));
    miscount_first_label(&mut chunk);
    let stored = MetaBytes::parse(GROUP, &chunk).unwrap();
    assert_eq!(stored.label_set(9).unwrap(), Some(LabelSetId(2)));
    let canonical = chunk_of(&BTreeMap::from([(3, 1), (9, 2)]));
    assert_ne!(chunk, canonical);
}
