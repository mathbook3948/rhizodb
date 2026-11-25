use super::columns_model_tests::Model;
use super::columns_model_tests::Values;
use super::columns_model_tests::check;
use super::columns_model_tests::create;
use super::columns_model_tests::delete;
use super::columns_model_tests::open;
use super::columns_model_tests::set;
use super::columns_model_tests::value_at;
use super::columns_model_tests::values_of;
use super::*;
use crate::engine::sim_fs::Crash;
use crate::engine::sim_fs::Failure;
use crate::engine::sim_fs::SimFs;
use crate::storage::redb_backend::RedbStorage;
use crate::storage::sim_backend;
use crate::test_scale;

const GROUP: u64 = meta::SLOTS as u64;

type Work = fn(&mut GraphTx<<RedbStorage as Storage>::Write<'_>>, &mut Model);

fn prepare(fs: &SimFs, small: u64) -> Model {
    let graph = open(fs);
    let mut model = Model::default();
    let mut write = graph.write().unwrap();
    for id in 0..200 {
        create(&mut write, &mut model, id % 2 == 0, values_of(id * 7919));
    }
    write.skip_node_ids_to(GROUP - 20);
    model.next = GROUP - 20;
    for id in 0..60 {
        create(&mut write, &mut model, id % 3 == 0, values_of(id * 104_729));
    }
    write.commit().unwrap();
    for round in 0..small {
        let mut write = graph.write().unwrap();
        let id = round * 3 % 200;
        set(&mut write, &mut model, id, "b", Some(value_at(round as u8)));
        write.commit().unwrap();
    }
    model
}

fn one_value(write: &mut GraphTx<<RedbStorage as Storage>::Write<'_>>, model: &mut Model) {
    set(write, model, 4, "a", Some(PropertyValue::Integer(-77)));
}

fn one_node(write: &mut GraphTx<<RedbStorage as Storage>::Write<'_>>, model: &mut Model) {
    create(write, model, true, values_of(0x0102_0304));
    set(
        write,
        model,
        10,
        "c",
        Some(PropertyValue::String("graph".to_owned())),
    );
    set(write, model, 12, "b", None);
}

fn many_values(write: &mut GraphTx<<RedbStorage as Storage>::Write<'_>>, model: &mut Model) {
    let ids: Vec<u64> = model.nodes.keys().copied().collect();
    for id in ids {
        set(write, model, id, "a", Some(PropertyValue::Float(id as f64)));
    }
    for id in 0..300 {
        create(write, model, id % 2 == 1, values_of(id));
    }
}

fn delete_many(write: &mut GraphTx<<RedbStorage as Storage>::Write<'_>>, model: &mut Model) {
    let doomed: Vec<u64> = model.nodes.keys().copied().step_by(3).collect();
    for id in doomed {
        delete(write, model, id);
    }
}

fn empty_a_column(write: &mut GraphTx<<RedbStorage as Storage>::Write<'_>>, model: &mut Model) {
    let held: Vec<u64> = model
        .nodes
        .range(GROUP..2 * GROUP)
        .map(|(id, _)| *id)
        .collect();
    for id in held {
        set(write, model, id, "d", None);
    }
    set(write, model, 1, "d", Some(PropertyValue::Boolean(true)));
}

fn fold_deltas(write: &mut GraphTx<<RedbStorage as Storage>::Write<'_>>, model: &mut Model) {
    for id in (1..200).step_by(2).take(columns::MAX_DELTAS) {
        set(
            write,
            model,
            id,
            "b",
            Some(PropertyValue::String(id.to_string())),
        );
    }
}

fn modes(seeds: u64) -> Vec<Crash> {
    let mut modes = vec![Crash::DropUnsynced, Crash::KeepUnsynced];
    modes.extend((0..seeds).map(|seed| Crash::Reorder { seed }));
    modes
}

fn same(read: &GraphTx<impl ReadTransaction>, model: &Model) -> bool {
    check(read, model).is_ok()
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
    let seeds = u64::from(test_scale::cases(2));
    let mut outcomes = BTreeSet::new();
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
                let as_before = same(&read, &before);
                let as_after = same(&read, &after);
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
                create(
                    &mut write,
                    &mut next,
                    true,
                    Values::from([("a", PropertyValue::Integer(1))]),
                );
                let first = *next.nodes.keys().next().unwrap();
                set(
                    &mut write,
                    &mut next,
                    first,
                    "c",
                    Some(PropertyValue::String("after".to_owned())),
                );
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
fn a_crash_inside_a_commit_of_one_value_leaves_the_graph_before_or_after_it() {
    crash_at_every_operation(0, one_value);
}

#[test]
fn a_crash_inside_a_commit_of_one_node_after_small_commits_leaves_the_graph_before_or_after_it() {
    crash_at_every_operation(40, one_node);
}

#[test]
fn a_crash_inside_a_commit_of_many_values_leaves_the_graph_before_or_after_it() {
    crash_at_every_operation(10, many_values);
}

#[test]
fn a_crash_inside_a_commit_that_deletes_many_nodes_leaves_the_graph_before_or_after_it() {
    crash_at_every_operation(10, delete_many);
}

#[test]
fn a_crash_inside_a_commit_that_empties_a_column_leaves_the_graph_before_or_after_it() {
    crash_at_every_operation(10, empty_a_column);
}

#[test]
fn a_crash_inside_a_commit_that_folds_deltas_into_segments_leaves_the_graph_before_or_after_it() {
    crash_at_every_operation(40, fold_deltas);
}

#[test]
fn no_writer_after_a_commit_that_failed_breaks_the_delta_limit_of_a_group() {
    let prepared = SimFs::new();
    let before = prepare(&prepared, 40);
    let operations = {
        let fs = prepared.crash(Crash::KeepUnsynced);
        let graph = open(&fs);
        let mut write = graph.write().unwrap();
        fold_deltas(&mut write, &mut before.clone());
        let start = fs.operations();
        write.commit().unwrap();
        fs.operations() - start
    };
    for at in 0..=operations {
        let fs = prepared.crash(Crash::KeepUnsynced);
        let graph = open(&fs);
        let mut write = graph.write().unwrap();
        fold_deltas(&mut write, &mut before.clone());
        fs.fail_after(at, Failure::Once);
        if write.commit().is_ok() {
            continue;
        }
        for round in 0..columns::MAX_DELTAS as u64 + 2 {
            let Ok(mut write) = graph.write() else {
                break;
            };
            let id = round * 2 + 1;
            let value = PropertyValue::Integer(-(round as i64));
            let wrote = write.set_node_property(NodeId(id), b_token(&write), value);
            if wrote.is_err() || write.commit().is_err() {
                break;
            }
        }
        if let Ok(read) = graph.read()
            && let Ok(violations) = read.check_integrity()
        {
            assert_eq!(
                violations,
                vec![],
                "after a failure at {at} of {operations}"
            );
        }
    }
}

fn b_token(write: &GraphTx<<RedbStorage as Storage>::Write<'_>>) -> Token<PropertyKey> {
    write.lookup_token::<PropertyKey>("b").unwrap().unwrap()
}
