use super::adjacency_model_tests::Model;
use super::adjacency_model_tests::Write;
use super::adjacency_model_tests::check;
use super::adjacency_model_tests::check_sides;
use super::adjacency_model_tests::create_node;
use super::adjacency_model_tests::create_rel;
use super::adjacency_model_tests::delete_rel;
use super::adjacency_model_tests::detach_delete;
use super::adjacency_model_tests::open;
use super::*;
use crate::engine::sim_fs::Crash;
use crate::engine::sim_fs::Failure;
use crate::engine::sim_fs::SimFs;
use crate::storage::test_engine;
use crate::test_scale;

const GROUP: u64 = meta::SLOTS as u64;

type Work = fn(&mut Write<'_>, &mut Model);

fn nodes(model: &Model) -> Vec<u64> {
    model.nodes.iter().copied().collect()
}

fn prepare(fs: &SimFs, small: u64) -> Model {
    let graph = open(fs);
    let mut model = Model::default();
    let mut write = graph.write().unwrap();
    for at in (0..70).chain(GROUP - 20..GROUP + 20).chain([3 * GROUP + 5]) {
        create_node(&mut write, &mut model, at);
    }
    let ids = nodes(&model);
    for (at, source) in ids.iter().enumerate() {
        for step in [1, 7, 30] {
            let target = ids[(at + step) % ids.len()];
            create_rel(&mut write, &mut model, *source, target, step % 3);
        }
    }
    let hub = 3 * GROUP + 5;
    for _ in 0..8 {
        for other in &ids {
            create_rel(&mut write, &mut model, hub, *other, 1);
        }
    }
    write.commit().unwrap();
    for round in 0..small {
        let mut write = graph.write().unwrap();
        let source = ids[(round * 5 % ids.len() as u64) as usize];
        let target = ids[(round * 11 % ids.len() as u64) as usize];
        create_rel(&mut write, &mut model, source, target, 0);
        write.commit().unwrap();
    }
    model
}

fn one_relationship(write: &mut Write<'_>, model: &mut Model) {
    create_rel(write, model, 3, GROUP + 2, 2);
}

fn one_delete(write: &mut Write<'_>, model: &mut Model) {
    let first = *model.rels.keys().next().unwrap();
    delete_rel(write, model, first);
}

fn self_loops_and_parallels(write: &mut Write<'_>, model: &mut Model) {
    create_rel(write, model, 5, 5, 0);
    create_rel(write, model, 5, 6, 0);
    create_rel(write, model, 5, 6, 0);
    create_rel(write, model, GROUP - 1, GROUP, 1);
}

fn many_relationships(write: &mut Write<'_>, model: &mut Model) {
    let ids = nodes(model);
    for (at, source) in ids.iter().enumerate() {
        create_rel(write, model, *source, ids[(at * 13 + 3) % ids.len()], 2);
    }
    let fresh = create_node(write, model, 6 * GROUP);
    for target in ids.iter().step_by(3) {
        create_rel(write, model, fresh, *target, 0);
    }
}

fn detach_many(write: &mut Write<'_>, model: &mut Model) {
    for node in nodes(model).into_iter().step_by(4) {
        detach_delete(write, model, node);
    }
}

fn fold_deltas(write: &mut Write<'_>, model: &mut Model) {
    for source in 0..adjacency::MAX_DELTAS as u64 + 1 {
        create_rel(write, model, source, 60, 0);
    }
}

fn grow_the_hub(write: &mut Write<'_>, model: &mut Model) {
    let hub = 3 * GROUP + 5;
    for other in nodes(model).into_iter().step_by(2) {
        create_rel(write, model, hub, other, 1);
    }
}

fn empty_a_segment(write: &mut Write<'_>, model: &mut Model) {
    let doomed: Vec<u64> = model
        .rels
        .iter()
        .filter(|(_, (source, _, index))| *source < 64 && *index == 1)
        .map(|(id, _)| *id)
        .collect();
    for id in doomed {
        delete_rel(write, model, id);
    }
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
    assert_ne!(before, after);
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
                let graph = match test_engine::on_sim(&recovered)
                    .map_err(GraphError::from)
                    .and_then(Graph::open)
                {
                    Ok(graph) => graph,
                    Err(error) => panic!("{failure:?} at {at} of {operations}, {mode:?}: {error}"),
                };
                let read = graph.read().unwrap();
                let as_before = check_sides(&read, &before).is_ok();
                let as_after = !as_before && check_sides(&read, &after).is_ok();
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
                let first = *next.nodes.iter().next().unwrap();
                let last = *next.nodes.iter().next_back().unwrap();
                create_rel(&mut write, &mut next, first, last, 2);
                let doomed = *next.rels.keys().next().unwrap();
                delete_rel(&mut write, &mut next, doomed);
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
fn a_crash_inside_a_commit_of_one_relationship_leaves_the_graph_before_or_after_it() {
    crash_at_every_operation(0, one_relationship);
}

#[test]
fn a_crash_inside_a_commit_of_one_relationship_after_small_commits_leaves_the_graph_before_or_after_it()
 {
    crash_at_every_operation(30, one_relationship);
}

#[test]
fn a_crash_inside_a_commit_of_one_delete_leaves_the_graph_before_or_after_it() {
    crash_at_every_operation(10, one_delete);
}

#[test]
fn a_crash_inside_a_commit_of_self_loops_and_parallel_relationships_leaves_the_graph_before_or_after_it()
 {
    crash_at_every_operation(10, self_loops_and_parallels);
}

#[test]
fn a_crash_inside_a_commit_of_many_relationships_leaves_the_graph_before_or_after_it() {
    crash_at_every_operation(10, many_relationships);
}

#[test]
fn a_crash_inside_a_commit_that_detaches_and_deletes_many_nodes_leaves_the_graph_before_or_after_it()
 {
    crash_at_every_operation(10, detach_many);
}

#[test]
fn a_crash_inside_a_commit_that_folds_deltas_into_parts_leaves_the_graph_before_or_after_it() {
    crash_at_every_operation(30, fold_deltas);
}

#[test]
fn a_crash_inside_a_commit_that_grows_a_hub_leaves_the_graph_before_or_after_it() {
    crash_at_every_operation(10, grow_the_hub);
}

#[test]
fn a_crash_inside_a_commit_that_empties_a_segment_leaves_the_graph_before_or_after_it() {
    crash_at_every_operation(10, empty_a_segment);
}
