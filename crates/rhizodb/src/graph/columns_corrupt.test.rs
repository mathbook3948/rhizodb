use super::columns_model_tests::KEYS;
use super::columns_model_tests::Model;
use super::columns_model_tests::check;
use super::columns_model_tests::create;
use super::columns_model_tests::open;
use super::columns_model_tests::set;
use super::columns_model_tests::values_of;
use super::*;
use crate::engine::sim_fs::SimFs;
use crate::storage::test_engine::TestStorage;
use crate::test_scale;
use crate::value::tests::same_bits;

const GROUP: u64 = meta::SLOTS as u64;

type Entries = Vec<(Vec<u8>, Vec<u8>)>;

fn graph_with_segments_and_deltas<S: Storage>(graph: &Graph<S>) -> Model {
    let mut model = Model::default();
    let mut write = graph.write().unwrap();
    for id in 0..150u64 {
        create(
            &mut write,
            &mut model,
            id % 2 == 0,
            values_of(id.wrapping_mul(0x9e37_79b9)),
        );
    }
    write.skip_node_ids_to(GROUP);
    model.next = GROUP;
    for id in 0..70u64 {
        let mut values = values_of(id);
        values.insert("a", PropertyValue::Integer(id as i64));
        values.insert("b", PropertyValue::Boolean(id % 3 == 0));
        values.insert("c", PropertyValue::String(format!("c{id}")));
        values.insert("d", PropertyValue::Float(id as f64 / 2.0));
        create(&mut write, &mut model, true, values);
    }
    write.commit().unwrap();
    let edits: [(u64, &str, Option<PropertyValue>); 5] = [
        (3, "a", Some(PropertyValue::Integer(33))),
        (5, "b", None),
        (
            GROUP + 2,
            "c",
            Some(PropertyValue::String("graph".to_owned())),
        ),
        (GROUP + 4, "a", None),
        (GROUP + 6, "d", Some(PropertyValue::Integer(6))),
    ];
    for (id, key, value) in edits {
        let mut write = graph.write().unwrap();
        set(&mut write, &mut model, id, key, value);
        write.commit().unwrap();
    }
    model
}

fn column_entries(read: &GraphTx<impl ReadTransaction>) -> Entries {
    read.transaction
        .scan_prefix(Table::NodeColumns, &[])
        .unwrap()
}

fn is_corruption(error: &GraphError) -> bool {
    matches!(error, GraphError::Corrupt(_) | GraphError::Decode(_))
}

fn answers_or_fails(read: &GraphTx<impl ReadTransaction>, model: &Model) -> usize {
    let mut failed = 0;
    let mut outcome = |result: Result<(), GraphError>| {
        if let Err(error) = result {
            assert!(is_corruption(&error), "{error:?} is reported as corruption");
            failed += 1;
        }
    };
    let tokens: Vec<(&str, Option<Token<PropertyKey>>)> = KEYS
        .iter()
        .map(|name| (*name, read.lookup_token::<PropertyKey>(name).unwrap()))
        .collect();
    let mut labels = LabelReader::default();
    let mut columns = ColumnReader::default();
    for (id, (_, values)) in &model.nodes {
        let node = NodeId(*id);
        outcome(read.node(node).map(|record| {
            let record = record.unwrap();
            assert_eq!(record.properties.len(), values.len(), "properties of {id}");
        }));
        for (name, token) in &tokens {
            let expected = values.get(name);
            for shared in [true, false] {
                let found = if shared {
                    read.node_property(&mut labels, &mut columns, node, *token)
                } else {
                    read.node_property(
                        &mut LabelReader::default(),
                        &mut ColumnReader::default(),
                        node,
                        *token,
                    )
                };
                outcome(found.map(|found| {
                    let found = found.unwrap();
                    let agrees = match (&found, expected) {
                        (Some(found), Some(expected)) => same_bits(found, expected),
                        (None, None) => true,
                        _ => false,
                    };
                    assert!(agrees, "{id}.{name}: {found:?} != {expected:?}");
                }));
            }
        }
    }
    failed
}

fn corrupt_and_check<S: Storage>(
    graph: &Graph<S>,
    model: &Model,
    key: &[u8],
    value: Option<&[u8]>,
) -> usize {
    let original = graph
        .read()
        .unwrap()
        .transaction
        .get(Table::NodeColumns, key)
        .unwrap();
    let put = |value: Option<&[u8]>| {
        let mut write = graph.write().unwrap();
        match value {
            Some(value) => write
                .transaction
                .put(Table::NodeColumns, key, value)
                .unwrap(),
            None => write.transaction.delete(Table::NodeColumns, key).unwrap(),
        }
        write.commit().unwrap();
    };
    put(value);
    let read = graph.read().unwrap();
    let failed = answers_or_fails(&read, model);
    let violations = read.check_integrity().unwrap();
    assert!(
        !violations.is_empty(),
        "the integrity check reports {key:?}"
    );
    drop(read);
    let (group, token) = match columns::parse_key(key).unwrap() {
        columns::ColumnKey::Segment { group, token, .. } => (group, token),
        columns::ColumnKey::Delta { group, .. } => {
            let read = graph.read().unwrap();
            (
                group,
                read.lookup_token::<PropertyKey>("a").unwrap().unwrap(),
            )
        }
    };
    let node = model.nodes.keys().find(|id| **id / GROUP == group).copied();
    if let Some(node) = node {
        let mut write = graph.write().unwrap();
        write.committing = true;
        let wrote = write
            .set_node_property(NodeId(node), token, PropertyValue::Integer(-1))
            .and_then(|()| write.flush_columns());
        if let Err(error) = wrote {
            assert!(is_corruption(&error), "{error:?} is reported as corruption");
        }
        write.rollback().unwrap();
    }
    put(original.as_deref());
    check(&graph.read().unwrap(), model).unwrap();
    failed
}

fn healthy() -> (SimFs, Graph<TestStorage>, Model) {
    let fs = SimFs::new();
    let graph = open(&fs);
    let model = graph_with_segments_and_deltas(&graph);
    check(&graph.read().unwrap(), &model).unwrap();
    (fs, graph, model)
}

fn positions(length: usize, head: u32) -> Vec<usize> {
    let head = test_scale::cases(head) as usize;
    let stride = (length / test_scale::cases(12) as usize).max(1);
    let mut positions: Vec<usize> = (0..head.min(length)).collect();
    positions.extend((head..length).step_by(stride));
    positions.push(length - 1);
    positions
}

#[test]
fn the_healthy_graph_keeps_segments_and_deltas_in_both_groups() {
    let (_fs, graph, _) = healthy();
    let read = graph.read().unwrap();
    let mut segments = BTreeSet::new();
    let mut deltas = Vec::new();
    for (key, _) in column_entries(&read) {
        match columns::parse_key(&key).unwrap() {
            columns::ColumnKey::Segment { group, segment, .. } => {
                segments.insert((group, segment));
            }
            columns::ColumnKey::Delta { group, slot } => deltas.push((group, slot)),
        }
    }
    assert_eq!(segments, [(0, 0), (0, 1), (0, 2), (1, 0), (1, 1)].into());
    deltas.sort_unstable();
    assert_eq!(deltas, vec![(0, 3), (0, 5), (1, 2), (1, 4), (1, 6)]);
}

#[test]
#[ignore = "left out of mise run check, run by mise run random"]
fn a_changed_byte_in_any_column_entry_fails_reads_that_need_it_and_never_misanswers() {
    let (_fs, graph, model) = healthy();
    let entries = column_entries(&graph.read().unwrap());
    for (key, value) in &entries {
        let mut failed = 0;
        for at in positions(value.len(), 12) {
            for flip in [0x01, 0x80] {
                let mut changed = value.clone();
                changed[at] ^= flip;
                failed += corrupt_and_check(&graph, &model, key, Some(&changed));
            }
        }
        assert!(failed > 0, "a damaged {key:?} fails some read");
    }
}

#[test]
fn a_cut_padded_or_emptied_column_entry_fails_reads_that_need_it() {
    let (_fs, graph, model) = healthy();
    let entries = column_entries(&graph.read().unwrap());
    for (key, value) in &entries {
        let mut padded = value.clone();
        padded.push(0);
        let changes = [
            value[..value.len() - 1].to_vec(),
            value[..value.len() / 2].to_vec(),
            Vec::new(),
            padded,
        ];
        for changed in changes {
            let failed = corrupt_and_check(&graph, &model, key, Some(&changed));
            assert!(failed > 0, "a cut or padded {key:?} fails some read");
        }
    }
}

#[test]
fn a_column_entry_stored_under_another_key_fails_reads_that_need_it() {
    let (_fs, graph, model) = healthy();
    let entries = column_entries(&graph.read().unwrap());
    for (key, _) in entries.iter().step_by(3) {
        for (_, other) in entries.iter().filter(|(held, _)| held != key).step_by(4) {
            let failed = corrupt_and_check(&graph, &model, key, Some(other));
            assert!(failed > 0, "{key:?} under another key fails some read");
        }
    }
}
