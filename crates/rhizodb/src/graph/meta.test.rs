use super::*;
use proptest::prelude::*;

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

proptest! {
    #[test]
    fn a_chunk_reads_back_each_slot_and_label_count_it_was_encoded_with(
        live in prop_oneof![live_slots(0..1), live_slots(1..3), live_slots(0..400)],
        label in (0u32..13).prop_map(Token::new),
    ) {
        let edit = edit_of(&live);
        let Some(chunk) = edit.encode(labels_of).unwrap() else {
            prop_assert!(live.is_empty());
            return Ok(());
        };
        let stored = MetaBytes::parse(&chunk).unwrap();
        for slot in 0..SLOTS {
            prop_assert_eq!(stored.label_set(slot), live.get(&slot).map(|set| LabelSetId(*set)));
        }
        let carrying = live
            .values()
            .filter(|set| labels_of(LabelSetId(**set)).unwrap().contains(&label))
            .count() as u64;
        prop_assert_eq!(stored.label_count(label), carrying);
        prop_assert_eq!(stored.to_edit().unwrap(), edit);
    }

    #[test]
    fn a_cut_or_padded_chunk_is_rejected(
        live in live_slots(0..400),
        cut in 1usize..64,
    ) {
        prop_assume!(!live.is_empty());
        let chunk = edit_of(&live).encode(labels_of).unwrap().unwrap();
        prop_assert!(MetaBytes::parse(&chunk[..chunk.len() - cut.min(chunk.len())]).is_err());
        let mut padded = chunk.clone();
        padded.push(0);
        prop_assert!(MetaBytes::parse(&padded).is_err());
    }
}

#[test]
fn a_group_of_one_label_set_stores_no_codes() {
    let edit = edit_of(&(0..SLOTS).map(|slot| (slot, 3)).collect());
    let chunk = edit.encode(labels_of).unwrap().unwrap();
    assert!(chunk.len() < 600);
}

#[test]
fn a_group_without_live_nodes_has_no_chunk() {
    let mut edit = edit_of(&BTreeMap::from([(7, 2)]));
    edit.remove(7);
    assert_eq!(edit.encode(labels_of).unwrap(), None);
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
fn meta_keys_order_by_group() {
    assert!(meta_key(1) < meta_key(2));
    assert!(meta_key(255) < meta_key(256));
    assert_eq!(parse_meta_key(&meta_key(9)).unwrap(), Some(9));
}
