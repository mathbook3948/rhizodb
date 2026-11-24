use std::collections::BTreeMap;

use proptest::prelude::*;
use proptest::test_runner::TestCaseError;

use super::*;

const ID: SegmentId = SegmentId {
    side: Side::Incoming,
    group: 3,
    segment: 5,
    rel_type: 9,
};

const LAST: usize = SEGMENT_SLOTS - 1;

const MIDDLE: usize = SEGMENT_SLOTS / 2;

fn is_corruption(error: &GraphError) -> bool {
    matches!(error, GraphError::Corrupt(_) | GraphError::Decode(_))
}

fn fails_with<T>(result: Result<T, GraphError>, expected: GraphError) -> bool {
    matches!(result, Err(error) if format!("{error:?}") == format!("{expected:?}"))
}

fn edge(relationship: u64, other: u64) -> Edge {
    Edge {
        relationship: RelationshipId(relationship),
        other: NodeId(other),
    }
}

fn borrowed(slots: &[(usize, Vec<Edge>)]) -> Vec<(usize, &[Edge])> {
    slots
        .iter()
        .map(|(position, edges)| (*position, edges.as_slice()))
        .collect()
}

fn encode(slots: &[(usize, Vec<Edge>)]) -> Vec<(Vec<u8>, Vec<u8>)> {
    encode_segment(&ID, &borrowed(slots)).unwrap()
}

fn decode(parts: &[(Vec<u8>, Vec<u8>)]) -> Vec<(usize, Vec<Edge>)> {
    let mut slots = Vec::new();
    for (key, bytes) in parts {
        slots.extend(PartRef::parse(key, bytes).unwrap().slots().unwrap());
    }
    slots
}

fn span_of(parts: &[(Vec<u8>, Vec<u8>)]) -> usize {
    match parse_key(&parts[0].0).unwrap().1 {
        Entry::Part { span, .. } => span,
        Entry::Delta(_) => panic!("a part key names a delta"),
    }
}

fn read_slot(
    stored: &BTreeMap<Vec<u8>, Vec<u8>>,
    position: usize,
) -> Result<Vec<Edge>, GraphError> {
    let mut read = SlotRead::default();
    let mut edges = Vec::new();
    for (key, value) in stored {
        let (_, entry) = parse_key(key)?;
        read.visit(entry, position, key, value, &mut |edge| edges.push(edge))?;
    }
    read.finish(&mut |edge| edges.push(edge))?;
    Ok(edges)
}

fn hub(edges: u64) -> Vec<Edge> {
    (0..edges)
        .map(|at| edge(1_000_000 + at * 3, at * 7919 % 1_000_003))
        .collect()
}

#[test]
fn a_segment_without_relationships_stores_no_part() {
    assert!(encode(&[]).is_empty());
    assert!(encode(&[(4, Vec::new())]).is_empty());
}

#[test]
fn a_segment_reads_back_every_slot_it_encodes() {
    let slots = vec![
        (0, vec![edge(0, 0)]),
        (1, vec![edge(5, 1), edge(6, 1), edge(9, 1)]),
        (MIDDLE, vec![edge(u64::MAX - 1, u64::MAX)]),
        (LAST, vec![edge(2, 4095), edge(3, 4096), edge(4, 4097)]),
    ];
    let parts = encode(&slots);
    assert_eq!(parts.len(), 1);
    assert_eq!(span_of(&parts), SEGMENT_SLOTS);
    assert_eq!(decode(&parts), slots);
}

#[test]
fn self_loops_and_parallel_relationships_keep_one_entry_each() {
    let node = ID.node_at(7).0;
    let slots = vec![(
        7,
        vec![edge(10, node), edge(11, node), edge(12, 99), edge(13, 99)],
    )];
    assert_eq!(decode(&encode(&slots)), slots);
}

#[test]
fn positions_at_the_edges_of_a_group_land_in_the_first_and_last_segment() {
    for (node, group, segment, position) in [
        (0, 0, 0, 0),
        (15, 0, 0, 15),
        (16, 0, 1, 0),
        (4095, 0, 255, 15),
        (4096, 1, 0, 0),
        (4097, 1, 0, 1),
    ] {
        let (id, at) = locate(Side::Outgoing, NodeId(node), 2);
        assert_eq!((id.group, id.segment, at), (group, segment, position));
        assert_eq!(id.node_at(at), NodeId(node));
    }
}

#[test]
fn a_full_segment_splits_into_parts_of_one_span_that_each_fit_a_page() {
    let slots: Vec<(usize, Vec<Edge>)> = (0..SEGMENT_SLOTS)
        .map(|position| {
            let first = position as u64 * 40;
            (
                position,
                (first..first + 20)
                    .map(|relationship| edge(relationship, relationship * 104_729))
                    .collect(),
            )
        })
        .collect();
    let parts = encode(&slots);
    assert!(parts.len() > 1);
    let span = span_of(&parts);
    for (key, bytes) in &parts {
        assert!(
            bytes.len() <= PART_BYTES,
            "a part holds {} bytes",
            bytes.len()
        );
        assert!(
            matches!(parse_key(key).unwrap().1, Entry::Part { span: held, .. } if held == span)
        );
    }
    let wider = encode_segment_by(&ID, &borrowed(&slots), span + 1);
    assert!(wider.iter().any(|(_, bytes)| bytes.len() > PART_BYTES));
    assert_eq!(decode(&parts), slots);
}

#[test]
fn a_hub_with_more_relationships_than_offsets_reach_gets_a_part_of_its_own() {
    let slots = vec![
        (0, vec![edge(1, 2)]),
        (MIDDLE, hub(70_000)),
        (LAST, vec![edge(5, 6)]),
    ];
    let parts = encode(&slots);
    assert_eq!(span_of(&parts), 1);
    assert_eq!(parts.len(), 3);
    let hub_part = &parts[1].1;
    assert!(hub_part.len() > usize::from(u16::MAX));
    assert_eq!(hub_part[FLAGS_AT], WIDE_OFFSETS);
    assert_eq!(decode(&parts), slots);
}

#[test]
fn wide_offsets_address_every_slot_of_a_part_past_sixty_four_kibibytes() {
    let slots = vec![(0, hub(20_000)), (1, vec![edge(7, 7)]), (2, hub(20_000))];
    let parts = encode_segment_by(&ID, &borrowed(&slots), SEGMENT_SLOTS);
    assert_eq!(parts.len(), 1);
    assert_eq!(parts[0].1[FLAGS_AT], WIDE_OFFSETS);
    let part = PartRef::parse(&parts[0].0, &parts[0].1).unwrap();
    assert_eq!(part.slot(1).unwrap(), vec![edge(7, 7)]);
    assert_eq!(part.slot(2).unwrap(), hub(20_000));
}

#[test]
fn slots_given_out_of_order_are_refused() {
    let out_of_order = vec![(5, vec![edge(1, 1)]), (4, vec![edge(2, 2)])];
    assert!(encode_segment(&ID, &borrowed(&out_of_order)).is_err());
    let descending = vec![(5, vec![edge(2, 1), edge(1, 1)])];
    assert!(encode_segment(&ID, &borrowed(&descending)).is_err());
    let repeated = vec![(5, vec![edge(2, 1), edge(2, 1)])];
    assert!(encode_segment(&ID, &borrowed(&repeated)).is_err());
    let outside = vec![(SEGMENT_SLOTS, vec![edge(2, 1)])];
    assert!(encode_segment(&ID, &borrowed(&outside)).is_err());
}

#[test]
fn every_damaged_byte_of_a_part_is_caught() {
    let parts = encode(&[
        (3, vec![edge(4, 5), edge(300, 70_000)]),
        (9, vec![edge(1, 1)]),
    ]);
    let (key, bytes) = &parts[0];
    for at in 0..bytes.len() {
        for flip in [0x01, 0x80, 0xff] {
            let mut damaged = bytes.clone();
            damaged[at] ^= flip;
            let error = PartRef::parse(key, &damaged).unwrap_err();
            assert!(is_corruption(&error), "byte {at}: {error:?}");
        }
    }
    for len in 0..bytes.len() {
        assert!(PartRef::parse(key, &bytes[..len]).is_err(), "cut at {len}");
    }
    let mut longer = bytes.clone();
    longer.push(0);
    assert!(PartRef::parse(key, &longer).is_err());
}

#[test]
fn a_part_or_delta_read_under_another_key_is_caught() {
    let parts = encode(&[(3, vec![edge(4, 5)])]);
    let (key, bytes) = &parts[0];
    let mut other = key.clone();
    other[GROUP_AT + 7] ^= 1;
    assert!(fails_with(PartRef::parse(&other, bytes), CHECKSUM_MISMATCH));
    let change = Change {
        removed: vec![RelationshipId(4)],
        added: vec![edge(8, 1)],
    };
    let delta = change.encode(&ID.delta_key(3));
    assert_eq!(Change::parse(&ID.delta_key(3), &delta).unwrap(), change);
    assert!(fails_with(
        Change::parse(&ID.delta_key(4), &delta),
        CHECKSUM_MISMATCH
    ));
}

#[test]
fn every_damaged_byte_of_a_delta_is_caught() {
    let key = ID.delta_key(12);
    let change = Change {
        removed: vec![RelationshipId(4), RelationshipId(900)],
        added: vec![edge(1_000, 3), edge(1_001, 300_000)],
    };
    let bytes = change.encode(&key);
    for at in 0..bytes.len() {
        let mut damaged = bytes.clone();
        damaged[at] ^= 0x40;
        let error = Change::parse(&key, &damaged).unwrap_err();
        assert!(is_corruption(&error), "byte {at}: {error:?}");
    }
    for len in 0..bytes.len() {
        assert!(Change::parse(&key, &bytes[..len]).is_err(), "cut at {len}");
    }
}

fn sealed_delta(key: &[u8], body: &[u8]) -> Vec<u8> {
    let mut bytes = vec![LAYOUT_VERSION, 0, 0, 0, 0];
    bytes.extend_from_slice(body);
    seal(key, &mut bytes);
    bytes
}

#[test]
fn a_delta_that_changes_nothing_or_is_not_in_shortest_form_is_refused() {
    let key = ID.delta_key(1);
    assert!(Change::parse(&key, &sealed_delta(&key, &[0, 0])).is_err());
    assert!(Change::parse(&key, &sealed_delta(&key, &[1, 0x85, 0x00, 0])).is_err());
    assert!(Change::parse(&key, &sealed_delta(&key, &[2, 5, 0, 0])).is_err());
    assert!(Change::parse(&key, &sealed_delta(&key, &[1, 5, 0, 7])).is_err());
    assert_eq!(
        Change::parse(&key, &sealed_delta(&key, &[2, 5, 1, 0])).unwrap(),
        Change {
            removed: vec![RelationshipId(5), RelationshipId(6)],
            added: Vec::new(),
        }
    );
}

#[test]
fn a_part_whose_offsets_run_backwards_or_past_its_end_is_refused() {
    let id = ID;
    let key = id.part_key(4, 0);
    for offsets in [[3u16, 2, 4], [1, 2, 9]] {
        let mut bytes = vec![LAYOUT_VERSION, 0, 0, 0, 0, 0];
        for offset in offsets {
            bytes.extend_from_slice(&offset.to_le_bytes());
        }
        bytes.extend_from_slice(&[1, 1, 2, 1]);
        seal(&key, &mut bytes);
        assert!(PartRef::parse(&key, &bytes).is_err(), "{offsets:?}");
    }
}

#[test]
fn keys_that_name_no_entry_are_refused() {
    let mut bad_side = ID.delta_key(0);
    bad_side[SIDE_AT] = 2;
    let mut bad_position = ID.delta_key(0);
    bad_position[KIND_AT + 1] = SEGMENT_SLOTS as u8;
    let mut bad_kind = ID.delta_key(0);
    bad_kind[KIND_AT] = 2;
    let zero_span = ID.part_key(0, 0);
    let past_last = ID.part_key(4, 4);
    let wide_span = ID.part_key(SEGMENT_SLOTS + 1, 0);
    for key in [
        bad_side.to_vec(),
        bad_position.to_vec(),
        bad_kind.to_vec(),
        zero_span.to_vec(),
        past_last.to_vec(),
        wide_span.to_vec(),
        ID.prefix().to_vec(),
        ID.part_key(1, 0)[..16].to_vec(),
    ] {
        assert!(parse_key(&key).is_err(), "{key:?}");
    }
    assert_eq!(
        parse_key(&ID.part_key(4, 3)).unwrap(),
        (ID, Entry::Part { span: 4, index: 3 })
    );
    assert_eq!(
        parse_key(&ID.delta_key(LAST)).unwrap(),
        (ID, Entry::Delta(LAST))
    );
}

fn store(entries: impl IntoIterator<Item = (Vec<u8>, Vec<u8>)>) -> BTreeMap<Vec<u8>, Vec<u8>> {
    entries.into_iter().collect()
}

#[test]
fn a_read_merges_the_delta_of_a_slot_with_its_part() {
    let mut stored = store(encode(&[(2, vec![edge(1, 10), edge(4, 11), edge(9, 12)])]));
    let change = Change {
        removed: vec![RelationshipId(4)],
        added: vec![edge(3, 20), edge(12, 21)],
    };
    stored.insert(ID.delta_key(2).to_vec(), change.encode(&ID.delta_key(2)));
    assert_eq!(
        read_slot(&stored, 2).unwrap(),
        vec![edge(1, 10), edge(3, 20), edge(9, 12), edge(12, 21)]
    );
    assert_eq!(read_slot(&stored, 3).unwrap(), Vec::new());
}

#[test]
fn a_delta_adds_to_a_slot_whose_part_is_not_stored() {
    let mut stored = store(encode(&[(2, vec![edge(1, 10)])]));
    let change = Change {
        removed: Vec::new(),
        added: vec![edge(5, 1)],
    };
    stored.insert(
        ID.delta_key(LAST).to_vec(),
        change.encode(&ID.delta_key(LAST)),
    );
    assert_eq!(read_slot(&stored, LAST).unwrap(), vec![edge(5, 1)]);
}

#[test]
fn a_delta_that_does_not_amend_its_segment_is_caught() {
    let parts = encode(&[(2, vec![edge(1, 10), edge(4, 11)])]);
    for (position, change) in [
        (
            2,
            Change {
                removed: vec![RelationshipId(3)],
                added: Vec::new(),
            },
        ),
        (
            2,
            Change {
                removed: Vec::new(),
                added: vec![edge(4, 11)],
            },
        ),
        (
            7,
            Change {
                removed: vec![RelationshipId(1)],
                added: Vec::new(),
            },
        ),
    ] {
        let mut stored = store(parts.clone());
        stored.insert(
            ID.delta_key(position).to_vec(),
            change.encode(&ID.delta_key(position)),
        );
        assert!(read_slot(&stored, position).is_err(), "{change:?}");
        let error = SegmentEdit::load(
            &ID,
            stored.iter().map(|(k, v)| (k.as_slice(), v.as_slice())),
        )
        .unwrap_err();
        assert!(is_corruption(&error));
    }
}

#[test]
fn a_delta_without_parts_is_caught() {
    let change = Change {
        removed: Vec::new(),
        added: vec![edge(5, 1)],
    };
    let stored = store([(ID.delta_key(1).to_vec(), change.encode(&ID.delta_key(1)))]);
    assert!(fails_with(read_slot(&stored, 1), DELTA_WITHOUT_PARTS));
    assert!(fails_with(
        SegmentEdit::load(
            &ID,
            stored.iter().map(|(k, v)| (k.as_slice(), v.as_slice()))
        ),
        DELTA_WITHOUT_PARTS
    ));
}

#[test]
fn parts_that_disagree_on_their_span_are_caught() {
    let slots = vec![(0, vec![edge(1, 1)]), (MIDDLE, vec![edge(2, 2)])];
    let mut stored = store(encode_segment_by(&ID, &borrowed(&slots), SEGMENT_SLOTS));
    stored.extend(encode_segment_by(&ID, &borrowed(&slots), MIDDLE));
    assert!(fails_with(read_slot(&stored, 0), SPANS_DISAGREE));
    assert!(fails_with(
        SegmentEdit::load(
            &ID,
            stored.iter().map(|(k, v)| (k.as_slice(), v.as_slice()))
        ),
        SPANS_DISAGREE
    ));
}

fn apply(stored: &mut BTreeMap<Vec<u8>, Vec<u8>>, edit: SegmentEdit) -> bool {
    let flush = edit.flush(&ID).unwrap();
    let is_part = |key: &[u8]| matches!(parse_key(key).unwrap().1, Entry::Part { .. });
    let rewrote = flush.delete.iter().any(|key| is_part(key))
        || flush.put.iter().any(|(key, _)| is_part(key));
    for key in flush.delete {
        assert!(stored.remove(&key).is_some(), "{key:?} is not stored");
    }
    stored.extend(flush.put);
    rewrote
}

fn load(stored: &BTreeMap<Vec<u8>, Vec<u8>>) -> SegmentEdit {
    SegmentEdit::load(
        &ID,
        stored.iter().map(|(k, v)| (k.as_slice(), v.as_slice())),
    )
    .unwrap()
}

fn seen(edit: &SegmentEdit, position: usize) -> Vec<Edge> {
    let mut edges = Vec::new();
    edit.visit(position, &mut |edge| edges.push(edge)).unwrap();
    edges
}

#[test]
fn a_commit_to_a_segment_without_parts_writes_parts() {
    let mut stored = BTreeMap::new();
    let mut edit = load(&stored);
    edit.add(3, edge(1, 2));
    assert!(apply(&mut stored, edit));
    assert_eq!(read_slot(&stored, 3).unwrap(), vec![edge(1, 2)]);
    assert!(
        stored
            .keys()
            .all(|key| matches!(parse_key(key).unwrap().1, Entry::Part { .. }))
    );
}

#[test]
fn small_commits_write_deltas_until_the_segment_would_keep_too_many() {
    let mut stored = store(encode(&[(0, vec![edge(0, 0)])]));
    for position in 1..=MAX_DELTAS {
        let mut edit = load(&stored);
        edit.add(position, edge(position as u64, 1));
        assert!(!apply(&mut stored, edit), "commit {position} rewrote");
    }
    assert_eq!(stored.len(), 1 + MAX_DELTAS);
    let mut edit = load(&stored);
    edit.add(MAX_DELTAS + 1, edge(100, 1));
    assert!(apply(&mut stored, edit));
    assert_eq!(stored.len(), 1);
    for position in 0..=MAX_DELTAS {
        assert_eq!(read_slot(&stored, position).unwrap().len(), 1);
    }
}

#[test]
fn a_delta_that_grows_past_its_bound_folds_into_the_parts() {
    let mut stored = store(encode(&[(0, vec![edge(0, 0)])]));
    let mut next = 1u64;
    let mut folded = false;
    for _ in 0..200 {
        let mut edit = load(&stored);
        for _ in 0..5 {
            edit.add(9, edge(next, next * 1_000_003));
            next += 1;
        }
        if apply(&mut stored, edit) {
            folded = true;
            break;
        }
        let (key, bytes) = stored
            .iter()
            .find(|(key, _)| matches!(parse_key(key).unwrap().1, Entry::Delta(_)))
            .unwrap();
        assert!(bytes.len() <= DELTA_BYTES, "{key:?} holds {}", bytes.len());
    }
    assert!(folded);
    assert_eq!(read_slot(&stored, 9).unwrap().len() as u64, next - 1);
}

#[test]
fn undoing_a_change_in_a_later_commit_deletes_its_delta() {
    let mut stored = store(encode(&[(0, vec![edge(0, 0), edge(1, 0)])]));
    let mut edit = load(&stored);
    edit.remove(0, RelationshipId(1)).unwrap();
    edit.add(0, edge(2, 5));
    assert!(!apply(&mut stored, edit));
    assert_eq!(stored.len(), 2);
    let mut edit = load(&stored);
    edit.remove(0, RelationshipId(2)).unwrap();
    assert_eq!(seen(&edit, 0), vec![edge(0, 0)]);
    assert!(!apply(&mut stored, edit));
    assert_eq!(read_slot(&stored, 0).unwrap(), vec![edge(0, 0)]);
    assert_eq!(stored.len(), 2);
}

#[test]
fn removing_a_relationship_a_slot_does_not_hold_is_corruption() {
    let stored = store(encode(&[(0, vec![edge(0, 0)])]));
    let mut edit = load(&stored);
    assert!(is_corruption(
        &edit.remove(0, RelationshipId(1)).unwrap_err()
    ));
    assert!(is_corruption(
        &edit.remove(5, RelationshipId(0)).unwrap_err()
    ));
    edit.remove(0, RelationshipId(0)).unwrap();
    assert!(is_corruption(
        &edit.remove(0, RelationshipId(0)).unwrap_err()
    ));
}

#[test]
fn removing_every_relationship_of_a_segment_deletes_it_once_it_folds() {
    let slots: Vec<(usize, Vec<Edge>)> = (0..SEGMENT_SLOTS)
        .map(|at| (at, vec![edge(at as u64, 1)]))
        .collect();
    let mut stored = store(encode(&slots));
    let mut edit = load(&stored);
    for at in 0..SEGMENT_SLOTS {
        edit.remove(at, RelationshipId(at as u64)).unwrap();
    }
    assert!(apply(&mut stored, edit));
    assert!(stored.is_empty());
}

#[derive(Clone, Debug)]
enum Op {
    Add(usize, u64),
    Remove(usize, usize),
    Commit,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => (0..SEGMENT_SLOTS, 0u64..5_000).prop_map(|(at, other)| Op::Add(at, other)),
        3 => (0..SEGMENT_SLOTS, any::<usize>()).prop_map(|(at, pick)| Op::Remove(at, pick)),
        1 => Just(Op::Commit),
    ]
}

fn hub_op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => (0..3usize, 0u64..5_000).prop_map(|(at, other)| Op::Add(at, other)),
        1 => (0..3usize, any::<usize>()).prop_map(|(at, pick)| Op::Remove(at, pick)),
        1 => Just(Op::Commit),
    ]
}

fn check_against_model(ops: &[Op], bulk: usize) -> Result<(), TestCaseError> {
    let mut model: BTreeMap<usize, Vec<Edge>> = BTreeMap::new();
    let mut stored = BTreeMap::new();
    let mut next = 0u64;
    for at in 0..bulk {
        model.entry(at % 3).or_default().push(edge(next, next % 97));
        next += 1;
    }
    let slots: Vec<(usize, Vec<Edge>)> = model.clone().into_iter().collect();
    stored.extend(encode(&slots));
    let mut edit = load(&stored);
    for op in ops {
        match op {
            Op::Add(at, other) => {
                let added = edge(next, *other);
                next += 1 + next % 3;
                edit.add(*at, added);
                model.entry(*at).or_default().push(added);
            }
            Op::Remove(at, pick) => {
                let Some(edges) = model.get_mut(at).filter(|edges| !edges.is_empty()) else {
                    continue;
                };
                let removed = edges.remove(pick % edges.len());
                edit.remove(*at, removed.relationship).unwrap();
            }
            Op::Commit => {
                apply(&mut stored, std::mem::take(&mut edit));
                let deltas = stored
                    .keys()
                    .filter(|key| matches!(parse_key(key).unwrap().1, Entry::Delta(_)))
                    .count();
                prop_assert!(deltas <= MAX_DELTAS);
                edit = load(&stored);
            }
        }
        for at in 0..SEGMENT_SLOTS {
            let expected = model.get(&at).cloned().unwrap_or_default();
            prop_assert_eq!(seen(&edit, at), expected);
        }
    }
    apply(&mut stored, edit);
    for at in 0..SEGMENT_SLOTS {
        let expected = model.get(&at).cloned().unwrap_or_default();
        prop_assert_eq!(read_slot(&stored, at).unwrap(), expected);
    }
    let parts: Vec<(Vec<u8>, Vec<u8>)> = stored
        .iter()
        .filter(|(key, _)| matches!(parse_key(key).unwrap().1, Entry::Part { .. }))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    let slots = decode(&parts);
    prop_assert_eq!(encode(&slots), parts);
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(crate::test_scale::cases(64)))]

    #[test]
    fn a_segment_edit_reads_and_stores_what_a_model_holds(ops in prop::collection::vec(op(), 0..120)) {
        check_against_model(&ops, 0)?;
    }

    #[test]
    fn a_segment_edit_keeps_hubs_as_a_model_does(ops in prop::collection::vec(hub_op(), 0..60), bulk in 0usize..3_000) {
        check_against_model(&ops, bulk)?;
    }

    #[test]
    fn any_segment_encodes_canonically_and_reads_back(
        slots in prop::collection::btree_map(
            0..SEGMENT_SLOTS,
            prop::collection::btree_map(any::<u64>(), any::<u64>(), 1..40),
            0..=SEGMENT_SLOTS,
        )
    ) {
        let slots: Vec<(usize, Vec<Edge>)> = slots
            .into_iter()
            .map(|(at, edges)| (at, edges.into_iter().map(|(rel, other)| edge(rel, other)).collect()))
            .collect();
        let parts = encode(&slots);
        let span = if parts.is_empty() { 1 } else { span_of(&parts) };
        for (key, bytes) in &parts {
            prop_assert!(span == 1 || bytes.len() <= PART_BYTES);
            let held = matches!(parse_key(key).unwrap().1, Entry::Part { span: held, .. } if held == span);
            prop_assert!(held);
        }
        prop_assert_eq!(decode(&parts), slots.clone());
        let stored = store(parts);
        for (at, edges) in &slots {
            prop_assert_eq!(&read_slot(&stored, *at).unwrap(), edges);
        }
    }

    #[test]
    fn a_damaged_part_never_reads_as_other_relationships(
        slots in prop::collection::btree_map(
            0..SEGMENT_SLOTS,
            prop::collection::btree_map(0u64..100_000, 0u64..100_000, 1..8),
            1..20,
        ),
        at in any::<usize>(),
        flip in 1u8..,
    ) {
        let slots: Vec<(usize, Vec<Edge>)> = slots
            .into_iter()
            .map(|(at, edges)| (at, edges.into_iter().map(|(rel, other)| edge(rel, other)).collect()))
            .collect();
        let parts = encode(&slots);
        let (key, bytes) = &parts[at % parts.len()];
        let mut damaged = bytes.clone();
        let byte = at % damaged.len();
        damaged[byte] ^= flip;
        prop_assert!(PartRef::parse(key, &damaged).is_err());
    }
}

#[test]
fn a_small_commit_that_removes_the_last_relationships_deletes_the_segment() {
    let mut stored = store(encode(&[
        (0, vec![edge(0, 0)]),
        (9, vec![edge(1, 0), edge(2, 0)]),
    ]));
    let mut edit = load(&stored);
    edit.remove(9, RelationshipId(2)).unwrap();
    assert!(!apply(&mut stored, edit));
    let mut edit = load(&stored);
    edit.remove(0, RelationshipId(0)).unwrap();
    assert!(!apply(&mut stored, edit));
    let mut edit = load(&stored);
    edit.remove(9, RelationshipId(1)).unwrap();
    assert!(apply(&mut stored, edit));
    assert!(stored.is_empty());
}

#[test]
fn a_fold_beside_a_hub_writes_only_the_parts_that_change() {
    let mut slots = vec![(0, hub(5_000))];
    slots.extend((1..SEGMENT_SLOTS).map(|at| (at, vec![edge(at as u64, 1)])));
    let mut stored = store(encode(&slots));
    let hub_key = ID.part_key(1, 0).to_vec();
    let hub_part = stored.get(&hub_key).cloned().unwrap();
    let mut edit = load(&stored);
    for at in 1..=MAX_DELTAS + 1 {
        edit.add(at, edge(100_000 + at as u64, 2));
    }
    let flush = edit.flush(&ID).unwrap();
    assert!(flush.put.iter().all(|(key, _)| *key != hub_key));
    assert!(flush.delete.iter().all(|key| *key != hub_key));
    assert!(!flush.put.is_empty());
    for key in flush.delete {
        stored.remove(&key);
    }
    stored.extend(flush.put);
    assert_eq!(stored.get(&hub_key), Some(&hub_part));
    assert_eq!(read_slot(&stored, 0).unwrap(), hub(5_000));
    assert_eq!(
        read_slot(&stored, 3).unwrap(),
        vec![edge(3, 1), edge(100_003, 2)]
    );
    let parts: Vec<Stored> = stored
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    assert_eq!(encode(&decode(&parts)), parts);
}
