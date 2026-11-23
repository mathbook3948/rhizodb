use super::node_meta_tests::LABELS;
use super::node_meta_tests::Model;
use super::node_meta_tests::check;
use super::node_meta_tests::create;
use super::node_meta_tests::labels_of;
use super::*;
use crate::engine::sim_fs::Crash;
use crate::engine::sim_fs::Failure;
use crate::engine::sim_fs::SimFs;
use crate::storage::redb_backend::RedbStorage;
use crate::storage::sim_backend;
use crate::test_scale;

const GROUP: u64 = meta::SLOTS as u64;

type Work = fn(&mut GraphTx<<RedbStorage as Storage>::Write<'_>>, &mut Model);

fn open(fs: &SimFs) -> Graph<RedbStorage> {
    Graph::open(sim_backend::storage(fs).unwrap()).unwrap()
}

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

fn prepare(fs: &SimFs, small: u64) -> Model {
    let graph = open(fs);
    let mut model = Model::default();
    let mut write = graph.write().unwrap();
    for slot in 0..160 {
        create(
            &mut write,
            &mut model,
            labels_of((slot % 7) as u8),
            Some(slot),
        );
    }
    write.skip_node_ids_to(GROUP - 20);
    model.next = GROUP - 20;
    for slot in 0..60 {
        create(&mut write, &mut model, labels_of((slot % 5) as u8), None);
    }
    write.commit().unwrap();
    for round in 0..small {
        let mut write = graph.write().unwrap();
        if round % 2 == 0 {
            relabel(&mut write, &mut model, round * 3, (round % 6) as usize);
        } else {
            create(&mut write, &mut model, labels_of(3), None);
        }
        write.commit().unwrap();
    }
    model.label_sets.clear();
    model
}

fn one_node(write: &mut GraphTx<<RedbStorage as Storage>::Write<'_>>, model: &mut Model) {
    create(write, model, labels_of(0b10_0001), Some(-1));
    relabel(write, model, 5, 4);
}

fn many_nodes(write: &mut GraphTx<<RedbStorage as Storage>::Write<'_>>, model: &mut Model) {
    for slot in 0..300 {
        create(write, model, labels_of((slot % 11) as u8), None);
    }
    let doomed: Vec<u64> = model.nodes.keys().copied().step_by(3).collect();
    for id in doomed {
        delete(write, model, id);
    }
}

fn empty_a_group(write: &mut GraphTx<<RedbStorage as Storage>::Write<'_>>, model: &mut Model) {
    let doomed: Vec<u64> = model
        .nodes
        .range(GROUP..2 * GROUP)
        .map(|(id, _)| *id)
        .collect();
    for id in doomed {
        delete(write, model, id);
    }
    relabel(write, model, 1, 0);
}

fn modes(seeds: u64) -> Vec<Crash> {
    let mut modes = vec![Crash::DropUnsynced, Crash::KeepUnsynced];
    modes.extend((0..seeds).map(|seed| Crash::Reorder { seed }));
    modes
}

fn crash_at_every_operation(small: u64, work: Work) {
    let prepared = SimFs::new();
    let before = prepare(&prepared, small);
    let mut after = before.clone();
    let operations = {
        let fs = prepared.crash(Crash::KeepUnsynced);
        let graph = open(&fs);
        let mut write = graph.write().unwrap();
        work(&mut write, &mut after);
        let start = fs.operations();
        write.commit().unwrap();
        fs.operations() - start
    };
    assert!(operations > 0);
    let seeds = u64::from(test_scale::cases(3));
    let mut outcomes = std::collections::BTreeSet::new();
    for failure in [Failure::FromThenOn, Failure::Once] {
        for at in 0..=operations {
            let fs = prepared.crash(Crash::KeepUnsynced);
            let committed = {
                let graph = open(&fs);
                let mut write = graph.write().unwrap();
                let mut scratch = before.clone();
                work(&mut write, &mut scratch);
                fs.fail_after(at, failure);
                write.commit().is_ok()
            };
            for mode in modes(seeds) {
                let recovered = fs.crash(mode);
                let graph = match sim_backend::storage(&recovered)
                    .map_err(GraphError::from)
                    .and_then(Graph::open)
                {
                    Ok(graph) => graph,
                    Err(error) => panic!("{failure:?} at {at} of {operations}, {mode:?}: {error}"),
                };
                let read = graph.read().unwrap();
                let as_before = check(&read, &before).is_ok();
                let as_after = check(&read, &after).is_ok();
                assert!(
                    as_before || as_after,
                    "{failure:?} at operation {at} of {operations}, {mode:?}: neither before nor after"
                );
                if committed {
                    assert!(
                        as_after,
                        "{failure:?} at {at}, {mode:?}: a commit that returned is lost"
                    );
                }
                outcomes.insert(as_after);
                drop(read);
                let mut next = if as_after {
                    after.clone()
                } else {
                    before.clone()
                };
                let mut write = graph.write().unwrap();
                create(&mut write, &mut next, labels_of(1), Some(7));
                write.commit().unwrap();
                check(&graph.read().unwrap(), &next).unwrap();
            }
        }
    }
    assert_eq!(
        outcomes,
        [false, true].into(),
        "the crashes land both before and after the commit"
    );
}

#[test]
fn a_crash_inside_a_one_node_commit_leaves_the_graph_before_or_after_it() {
    crash_at_every_operation(0, one_node);
}

#[test]
fn a_crash_inside_a_one_node_commit_after_small_commits_leaves_the_graph_before_or_after_it() {
    crash_at_every_operation(40, one_node);
}

#[test]
fn a_crash_inside_a_commit_of_many_nodes_leaves_the_graph_before_or_after_it() {
    crash_at_every_operation(10, many_nodes);
}

#[test]
fn a_crash_inside_a_commit_that_empties_a_group_leaves_the_graph_before_or_after_it() {
    crash_at_every_operation(10, empty_a_group);
}
