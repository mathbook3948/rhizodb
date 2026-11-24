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
        .create_node(NodeRecord {
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

#[test]
fn the_format_eight_fixtures_upgrade_to_consistent_graphs_with_no_node_record_left() {
    for fixture in ["format-8.redb", "format-8-db-file.redb"] {
        let directory = tempfile::tempdir().unwrap();
        let path = copy_of(fixture, directory.path());
        for _ in 0..2 {
            let graph = Graph::open(RedbStorage::open(&path).unwrap()).unwrap();
            let read = graph.read().unwrap();
            assert_eq!(read.check_integrity().unwrap(), vec![], "{fixture}");
            assert_ne!(
                read.transaction
                    .scan_prefix(Table::NodeColumns, &[])
                    .unwrap(),
                vec![],
                "{fixture}"
            );
            if let Ok(entries) = read.transaction.scan_prefix(Table::Nodes, &[]) {
                assert_eq!(entries, vec![], "{fixture}: the node records are emptied");
            }
        }
    }
}

fn every_node(read: &GraphTx<impl ReadTransaction>) -> Vec<(NodeId, NodeRecord)> {
    read.node_ids()
        .unwrap()
        .into_iter()
        .map(|id| (id, read.node(id).unwrap().unwrap()))
        .collect()
}

fn value_of(i: u64, key: u32) -> Option<PropertyValue> {
    let i = i as i64;
    match (key, i % 7) {
        (0, _) => Some(PropertyValue::Integer(i)),
        (1, 0) => None,
        (1, _) => Some(PropertyValue::Float(-0.0 * i as f64)),
        (2, 3) => Some(PropertyValue::Boolean(i % 2 == 0)),
        (2, 4) => Some(PropertyValue::List(vec![PropertyValue::Integer(i)])),
        (2, 5) => Some(PropertyValue::String(String::new())),
        (2, _) if (4096..8192).contains(&i) => Some(PropertyValue::String(format!("s{i}"))),
        (2, _) => Some(PropertyValue::Integer(-i)),
        (3, _) if i % 4096 == 4095 => Some(PropertyValue::Boolean(true)),
        _ => None,
    }
}

#[test]
fn a_graph_stored_as_format_eight_reads_back_every_node_after_the_upgrade() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("eight.redb");
    let before = {
        let graph = Graph::open(RedbStorage::open(&path).unwrap()).unwrap();
        let mut write = graph.write().unwrap();
        let label = write.intern_token::<Label>("N").unwrap();
        let keys: Vec<Token<PropertyKey>> = ["i", "f", "m", "edge"]
            .iter()
            .map(|name| write.intern_token(name).unwrap())
            .collect();
        for i in 0..12_300u64 {
            let mut record = NodeRecord::default();
            if i % 3 == 0 {
                record.labels.insert(label);
            }
            for (at, key) in keys.iter().enumerate() {
                if let Some(value) = value_of(i, at as u32) {
                    record.properties.insert(*key, value);
                }
            }
            write.create_node(record.clone()).unwrap();
        }
        for i in (4096..8192).filter(|i| i % 13 != 0).chain(12_000..12_300) {
            write.delete_node(NodeId(i)).unwrap().unwrap();
        }
        write.commit().unwrap();
        let before = every_node(&graph.read().unwrap());
        let mut previous = graph.storage.begin_write().unwrap();
        crate::graph::tests::store_as_format_eight(&mut previous);
        previous.commit().unwrap();
        before
    };
    assert_eq!(before.len(), 12_300 - 4096 + 315 - 300);
    for _ in 0..2 {
        let graph = Graph::open(RedbStorage::open(&path).unwrap()).unwrap();
        let read = graph.read().unwrap();
        assert_eq!(read.check_integrity().unwrap(), vec![]);
        let after = every_node(&read);
        assert_eq!(after.len(), before.len());
        for ((id, record), (stored_id, stored)) in after.iter().zip(&before) {
            assert_eq!(id, stored_id);
            assert_eq!(record.labels, stored.labels, "{id:?}");
            assert_eq!(record.properties.len(), stored.properties.len(), "{id:?}");
            for (key, value) in &record.properties {
                let held = &stored.properties[key];
                assert!(
                    crate::value::tests::same_bits(value, held),
                    "{id:?}: {value:?} {held:?}"
                );
            }
        }
    }
}

#[test]
fn a_graph_stored_as_format_nine_reads_back_every_relationship_after_the_upgrade() {
    use super::adjacency_model_tests::Model as Relationships;
    use super::adjacency_model_tests::check;
    use super::adjacency_model_tests::create_node;
    use super::adjacency_model_tests::create_rel;
    use super::adjacency_model_tests::delete_rel;
    use super::adjacency_model_tests::open;

    let fs = crate::engine::sim_fs::SimFs::new();
    let mut model = Relationships::default();
    {
        let graph = open(&fs);
        let mut write = graph.write().unwrap();
        let mut nodes = Vec::new();
        for at in (0..40).chain(4090..4100).chain([8191, 8192, 20_000]) {
            nodes.push(create_node(&mut write, &mut model, at));
        }
        let hub = 20_000;
        for _ in 0..90 {
            for node in &nodes {
                create_rel(&mut write, &mut model, hub, *node, 0);
                create_rel(&mut write, &mut model, *node, hub, 1);
            }
        }
        for (at, node) in nodes.iter().enumerate() {
            create_rel(&mut write, &mut model, *node, *node, 2);
            create_rel(
                &mut write,
                &mut model,
                *node,
                nodes[(at + 1) % nodes.len()],
                2,
            );
            create_rel(
                &mut write,
                &mut model,
                *node,
                nodes[(at + 1) % nodes.len()],
                2,
            );
        }
        write.commit().unwrap();
        for round in 0..8 {
            let mut write = graph.write().unwrap();
            create_rel(&mut write, &mut model, nodes[round], nodes[round * 3], 1);
            let doomed = *model.rels.keys().nth(round * 11).unwrap();
            delete_rel(&mut write, &mut model, doomed);
            write.commit().unwrap();
        }
        let mut previous = graph.storage.begin_write().unwrap();
        crate::graph::tests::store_as_format_nine(&mut previous);
        previous.commit().unwrap();
    }
    assert!(model.rels.len() > 4096 * 2);
    for _ in 0..2 {
        let graph = open(&fs);
        let read = graph.read().unwrap();
        check(&read, &model).unwrap();
        for table in [Table::Outgoing, Table::Incoming] {
            if let Ok(entries) = read.transaction.scan_prefix(table, &[]) {
                assert_eq!(entries, vec![], "{table:?} is emptied");
            }
        }
    }
}
