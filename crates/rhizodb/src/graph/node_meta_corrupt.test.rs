use super::node_meta_tests::LABELS;
use super::node_meta_tests::Model;
use super::node_meta_tests::check;
use super::node_meta_tests::create;
use super::node_meta_tests::ids_without_a_node_worth_asking;
use super::node_meta_tests::labels_of;
use super::node_meta_tests::names;
use super::node_meta_tests::token_names;
use super::*;
use crate::storage::redb_backend::RedbStorage;
use crate::test_scale;

const GROUP: u64 = meta::SLOTS as u64;

fn relabel(write: &mut GraphTx<impl WriteTransaction>, model: &mut Model, id: u64, label: usize) {
    let token = write.intern_token::<Label>(LABELS[label]).unwrap();
    let labels = &mut model.nodes.get_mut(&id).unwrap().0;
    if labels.remove(LABELS[label]) {
        write.remove_label(NodeId(id), token).unwrap();
    } else {
        labels.insert(LABELS[label]);
        write.add_label(NodeId(id), token).unwrap();
    }
}

fn delete(write: &mut GraphTx<impl WriteTransaction>, model: &mut Model, id: u64) {
    write.delete_node(NodeId(id)).unwrap().unwrap();
    model.nodes.remove(&id);
}

fn graph_with_chunks_and_deltas<S: Storage>(graph: &Graph<S>) -> Model {
    let mut model = Model::default();
    let mut write = graph.write().unwrap();
    for index in 0..150u8 {
        create(&mut write, &mut model, labels_of(index % 64), None);
    }
    write.skip_node_ids_to(GROUP);
    model.next = GROUP;
    for index in 0..20u8 {
        create(
            &mut write,
            &mut model,
            labels_of(index.wrapping_mul(7) % 64),
            None,
        );
    }
    write.commit().unwrap();
    let edits: [fn(&mut GraphTx<_>, &mut Model); 6] = [
        |write, model| delete(write, model, 3),
        |write, model| relabel(write, model, 5, 0),
        |write, model| relabel(write, model, 7, 5),
        |write, model| delete(write, model, GROUP + 2),
        |write, model| create(write, model, labels_of(9), None),
        |write, model| relabel(write, model, GROUP + 4, 2),
    ];
    for edit in edits {
        let mut write = graph.write().unwrap();
        edit(&mut write, &mut model);
        write.commit().unwrap();
    }
    model
}

fn node_meta(read: &GraphTx<impl ReadTransaction>) -> Entries {
    read.transaction
        .scan_prefix(Table::NodeChunks, &[])
        .unwrap()
}

fn is_corruption(error: &GraphError) -> bool {
    matches!(error, GraphError::Corrupt(_) | GraphError::Decode(_))
}

fn answers_or_fails(read: &GraphTx<impl ReadTransaction>, model: &Model, count_too: bool) -> usize {
    let mut failed = 0;
    let mut outcome = |result: Result<(), GraphError>| {
        if let Err(error) = result {
            assert!(is_corruption(&error), "{error:?} is reported as corruption");
            failed += 1;
        }
    };
    let names = names(read, &LABELS);
    outcome(read.node_ids().map(|ids| {
        let ids: Vec<u64> = ids.into_iter().map(|id| id.0).collect();
        assert_eq!(
            ids,
            model.nodes.keys().copied().collect::<Vec<_>>(),
            "node IDs"
        );
    }));
    let mut reader = LabelReader::default();
    let probes = ids_without_a_node_worth_asking(model);
    for id in model.nodes.keys().chain(&probes) {
        let expected = model.nodes.get(id).map(|(labels, _)| labels.clone());
        outcome(read.node_labels(NodeId(*id)).map(|found| {
            let found = found.map(|labels| token_names(&names, &labels));
            assert_eq!(found, expected, "labels of {id}");
        }));
        outcome(
            read.with_labels(&mut reader, NodeId(*id), |labels| {
                token_names(&names, labels)
            })
            .map(|found| assert_eq!(found, expected, "cached labels of {id}")),
        );
    }
    for name in LABELS {
        let Some(label) = read.lookup_token::<Label>(name).unwrap() else {
            continue;
        };
        let carrying: Vec<u64> = model
            .nodes
            .iter()
            .filter(|(_, (labels, _))| labels.contains(name))
            .map(|(id, _)| *id)
            .collect();
        outcome(read.node_ids_with_label(label).map(|ids| {
            let ids: Vec<u64> = ids.into_iter().map(|id| id.0).collect();
            assert_eq!(ids, carrying, "scan of {name}");
        }));
        if count_too {
            outcome(read.count_nodes_with_label(label).map(|count| {
                assert_eq!(count, carrying.len() as u64, "count of {name}");
            }));
        }
    }
    failed
}

fn corrupt_and_check<S: Storage>(
    graph: &Graph<S>,
    model: &Model,
    key: &[u8],
    value: Option<&[u8]>,
    count_too: bool,
) {
    let original = graph
        .read()
        .unwrap()
        .transaction
        .get(Table::NodeChunks, key)
        .unwrap();
    let put = |value: Option<&[u8]>| {
        let mut write = graph.write().unwrap();
        match value {
            Some(value) => write
                .transaction
                .put(Table::NodeChunks, key, value)
                .unwrap(),
            None => write.transaction.delete(Table::NodeChunks, key).unwrap(),
        }
        write.commit().unwrap();
    };
    put(value);
    let read = graph.read().unwrap();
    answers_or_fails(&read, model, count_too);
    let violations = read.check_integrity().unwrap();
    assert!(
        !violations.is_empty(),
        "the integrity check reports {key:?}"
    );
    drop(read);
    let (group, _) = meta::parse_key(key).unwrap().unwrap();
    if let Some(node) = model.nodes.keys().find(|id| **id / GROUP == group) {
        let mut write = graph.write().unwrap();
        let token = write.intern_token::<Label>("Z").unwrap();
        let error = write.add_label(NodeId(*node), token).unwrap_err();
        assert!(is_corruption(&error), "{error:?} is reported as corruption");
        write.rollback().unwrap();
    }
    put(original.as_deref());
    check(&graph.read().unwrap(), model).unwrap();
}

fn healthy() -> (Graph<RedbStorage>, Model) {
    let graph = Graph::open(RedbStorage::in_memory().unwrap()).unwrap();
    let model = graph_with_chunks_and_deltas(&graph);
    check(&graph.read().unwrap(), &model).unwrap();
    (graph, model)
}

fn positions(length: usize, head: usize) -> Vec<usize> {
    let stride = (length / test_scale::cases(40) as usize).max(1);
    let mut positions: Vec<usize> = (0..head.min(length)).collect();
    positions.extend((head..length).step_by(stride));
    positions.push(length - 1);
    positions
}

#[test]
fn the_healthy_graph_keeps_chunks_and_deltas_in_both_groups() {
    let (graph, _) = healthy();
    let read = graph.read().unwrap();
    let parts: Vec<(u64, meta::Part)> = node_meta(&read)
        .iter()
        .map(|(key, _)| meta::parse_key(key).unwrap().unwrap())
        .collect();
    assert_eq!(
        parts,
        vec![
            (0, meta::Part::Chunk),
            (0, meta::Part::Delta(3)),
            (0, meta::Part::Delta(5)),
            (0, meta::Part::Delta(7)),
            (1, meta::Part::Chunk),
            (1, meta::Part::Delta(2)),
            (1, meta::Part::Delta(4)),
            (1, meta::Part::Delta(20)),
        ]
    );
}

#[test]
fn a_changed_byte_in_any_node_meta_entry_fails_reads_that_need_it_and_never_misanswers() {
    let (graph, model) = healthy();
    let entries = node_meta(&graph.read().unwrap());
    for (key, value) in &entries {
        for at in positions(value.len(), 128) {
            for flip in [0x01, 0x80, 0xff] {
                let mut changed = value.clone();
                changed[at] ^= flip;
                corrupt_and_check(&graph, &model, key, Some(&changed), true);
            }
        }
    }
}

#[test]
fn a_cut_padded_or_emptied_node_meta_entry_fails_reads_that_need_it() {
    let (graph, model) = healthy();
    let entries = node_meta(&graph.read().unwrap());
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
            corrupt_and_check(&graph, &model, key, Some(&changed), true);
        }
    }
}

#[test]
fn a_node_meta_entry_stored_under_another_key_fails_reads_that_need_it() {
    let (graph, model) = healthy();
    let entries = node_meta(&graph.read().unwrap());
    for (key, _) in &entries {
        for (_, other) in entries.iter().filter(|(held, _)| held != key) {
            corrupt_and_check(&graph, &model, key, Some(other), true);
        }
    }
    let (_, delta) = &entries[1];
    for slot in [0, 4, 6, 200] {
        corrupt_and_check(&graph, &model, &meta::delta_key(0, slot), Some(delta), true);
    }
}

#[test]
fn deltas_left_without_their_chunk_fail_reads_that_need_them() {
    let (graph, model) = healthy();
    for group in [0, 1] {
        corrupt_and_check(&graph, &model, &meta::meta_key(group), None, true);
    }
}

#[test]
fn a_sealed_delta_that_does_not_amend_its_chunk_misleads_no_read_but_the_count() {
    let (graph, model) = healthy();
    let wrong = [
        (3, Some(LabelSetId(1)), None),
        (4, None, Some(LabelSetId(1))),
        (6, Some(LabelSetId(1)), Some(LabelSetId(2))),
    ];
    for (slot, before, after) in wrong {
        let delta = meta::Delta {
            slot,
            before,
            after,
        };
        corrupt_and_check(
            &graph,
            &model,
            &meta::delta_key(0, slot),
            Some(&delta.encode(0)),
            false,
        );
    }
}

type Entries = Vec<(Vec<u8>, Vec<u8>)>;

fn file_with_graph(dir: &std::path::Path) -> (std::path::PathBuf, Model, Entries) {
    let path = dir.join("graph.redb");
    let graph = Graph::open(RedbStorage::open(&path).unwrap()).unwrap();
    let model = graph_with_chunks_and_deltas(&graph);
    let entries = node_meta(&graph.read().unwrap());
    drop(graph);
    (path, model, entries)
}

fn open_answers_or_fails(path: &std::path::Path, model: &Model) {
    let Ok(storage) = RedbStorage::open(path) else {
        return;
    };
    let Ok(graph) = Graph::open(storage) else {
        return;
    };
    let Ok(read) = graph.read() else {
        return;
    };
    answers_or_fails(&read, model, true);
}

#[test]
fn a_changed_byte_of_node_meta_in_the_database_file_fails_reads_or_answers_as_before() {
    let dir = tempfile::tempdir().unwrap();
    let (path, model, entries) = file_with_graph(dir.path());
    let pristine = std::fs::read(&path).unwrap();
    let mut changed_any = 0;
    for (_, value) in &entries {
        let found: Vec<usize> = pristine
            .windows(value.len())
            .enumerate()
            .filter(|(_, window)| *window == value.as_slice())
            .map(|(at, _)| at)
            .collect();
        assert!(!found.is_empty(), "the file holds each node meta entry");
        for start in found {
            for at in positions(value.len(), 32) {
                let mut file = pristine.clone();
                file[start + at] ^= 0x5a;
                std::fs::write(&path, &file).unwrap();
                open_answers_or_fails(&path, &model);
                changed_any += 1;
            }
        }
    }
    assert!(changed_any > 0);
}
