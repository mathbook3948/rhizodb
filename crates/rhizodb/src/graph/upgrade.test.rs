use std::path::Path;
use std::path::PathBuf;

use super::node_meta_tests::Labels;
use super::node_meta_tests::Model;
use super::node_meta_tests::check_with;
use super::*;
use crate::storage::redb_backend::RedbStorage;

const WIDE_LABELS: [&str; 13] = [
    "Wide", "Sparse", "Gone", "Tail", "L0", "L1", "L2", "L3", "L4", "L5", "L6", "L7", "L8",
];
const BITS: [&str; 9] = ["L0", "L1", "L2", "L3", "L4", "L5", "L6", "L7", "L8"];

fn copy_of(fixture: &str, directory: &Path) -> PathBuf {
    let path = directory.join("graph.redb");
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(fixture),
        &path,
    )
    .unwrap();
    path
}

fn wide_model() -> Model {
    let mut model = Model::default();
    let mut put = |id: u64, labels: Labels| {
        model.nodes.insert(id, (labels, Some(id as i64)));
    };
    for i in (0..1200).filter(|i| i % 7 != 3) {
        let mut labels: Labels = BITS
            .iter()
            .enumerate()
            .filter(|(bit, _)| i & (1 << bit) != 0)
            .map(|(_, name)| *name)
            .collect();
        if i % 11 != 5 && i != 1024 {
            labels.insert("Wide");
        }
        put(i, labels);
    }
    put(4095, Labels::from(["Wide", "L8"]));
    for i in [8192, 8193, 9000, 11000, 12000, 12286, 12287] {
        put(i, Labels::from(["Wide", "Sparse"]));
    }
    put(10000, Labels::new());
    put(12288, Labels::from(["Tail"]));
    put(12289, Labels::from(["Tail", "L0"]));
    model.next = 12290;
    model
}

#[test]
fn the_wide_format_seven_fixture_upgrades_to_the_graph_it_was_written_as() {
    let directory = tempfile::tempdir().unwrap();
    let path = copy_of("format-7-wide.redb", directory.path());
    let model = wide_model();
    for _ in 0..2 {
        let graph = Graph::open(RedbStorage::open(&path).unwrap()).unwrap();
        check_with(&graph.read().unwrap(), &model, &WIDE_LABELS, "i").unwrap();
    }
    let read = Graph::open(RedbStorage::open(&path).unwrap()).unwrap();
    let read = read.read().unwrap();
    if let Ok(entries) = read.transaction.scan_prefix(Table::LabelIndex, &[]) {
        assert_eq!(entries, vec![], "the label index is emptied");
    }
}

#[test]
fn the_wide_format_seven_fixture_takes_writes_in_every_group_after_the_upgrade() {
    let directory = tempfile::tempdir().unwrap();
    let path = copy_of("format-7-wide.redb", directory.path());
    let mut model = wide_model();
    let graph = Graph::open(RedbStorage::open(&path).unwrap()).unwrap();
    let mut write = graph.write().unwrap();
    let key = write.intern_token::<PropertyKey>("i").unwrap();
    let tail = write.intern_token::<Label>("Tail").unwrap();
    let wide = write.intern_token::<Label>("Wide").unwrap();
    for id in [4, 4095, 8193, 12289] {
        write.add_label(NodeId(id), tail).unwrap();
        model.nodes.get_mut(&id).unwrap().0.insert("Tail");
    }
    for id in [0, 9000, 10000] {
        write.delete_node(NodeId(id)).unwrap().unwrap();
        model.nodes.remove(&id);
    }
    let created = write
        .create_node(&NodeRecord {
            labels: [wide].into(),
            properties: [(key, PropertyValue::Integer(12290))].into(),
        })
        .unwrap();
    assert_eq!(
        created,
        NodeId(12290),
        "IDs go on from the upgraded counter"
    );
    model
        .nodes
        .insert(12290, (Labels::from(["Wide"]), Some(12290)));
    write.commit().unwrap();
    drop(graph);
    let graph = Graph::open(RedbStorage::open(&path).unwrap()).unwrap();
    check_with(&graph.read().unwrap(), &model, &WIDE_LABELS, "i").unwrap();
}

#[test]
fn the_basic_format_seven_fixture_upgrades_to_a_consistent_graph() {
    let directory = tempfile::tempdir().unwrap();
    let path = copy_of("format-7.redb", directory.path());
    let graph = Graph::open(RedbStorage::open(&path).unwrap()).unwrap();
    assert_eq!(graph.read().unwrap().check_integrity().unwrap(), vec![]);
}
