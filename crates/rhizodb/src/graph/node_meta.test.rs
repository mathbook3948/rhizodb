use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use proptest::prelude::*;
use proptest::test_runner::TestCaseError;

use super::*;
use crate::engine::sim_fs::Crash;
use crate::engine::sim_fs::SimFs;
use crate::storage::redb_backend::RedbStorage;
use crate::storage::sim_backend;
use crate::test_scale;

pub(super) const LABELS: [&str; 6] = ["A", "B", "C", "D", "E", "F"];
pub(super) const VALUE: &str = "v";
const GROUP: u64 = meta::SLOTS as u64;

pub(super) type Labels = BTreeSet<&'static str>;

/// The nodes of a graph as the tests see them: each live node's labels and the value of `v`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Model {
    pub(super) nodes: BTreeMap<u64, (Labels, Option<i64>)>,
    pub(super) next: u64,
    pub(super) label_sets: BTreeSet<Labels>,
}

impl Model {
    fn commit(&mut self) {
        for (labels, _) in self.nodes.values() {
            if !labels.is_empty() {
                self.label_sets.insert(labels.clone());
            }
        }
    }

    pub(super) fn create(&mut self, labels: Labels, value: Option<i64>) -> u64 {
        let id = self.next;
        self.next += 1;
        self.nodes.insert(id, (labels, value));
        id
    }

    fn pick(&self, index: usize) -> Option<u64> {
        let len = self.nodes.len();
        (len > 0).then(|| *self.nodes.keys().nth(index % len).unwrap_or(&0))
    }

    fn relabel(&mut self, id: u64, edit: impl FnOnce(&mut Labels)) {
        let mut labels = self.nodes[&id].0.clone();
        edit(&mut labels);
        self.nodes.get_mut(&id).unwrap().0 = labels;
    }
}

pub(super) fn labels_of(mask: u8) -> Labels {
    LABELS
        .iter()
        .enumerate()
        .filter(|(bit, _)| mask & (1 << bit) != 0)
        .map(|(_, name)| *name)
        .collect()
}

fn record(
    write: &mut GraphTx<impl WriteTransaction>,
    labels: &Labels,
    value: Option<i64>,
) -> NodeRecord {
    let labels = labels
        .iter()
        .map(|name| write.intern_token::<Label>(name).unwrap())
        .collect();
    let mut properties = Properties::new();
    if let Some(value) = value {
        let key = write.intern_token::<PropertyKey>(VALUE).unwrap();
        properties.insert(key, PropertyValue::Integer(value));
    }
    NodeRecord { labels, properties }
}

pub(super) fn create(
    write: &mut GraphTx<impl WriteTransaction>,
    model: &mut Model,
    labels: Labels,
    value: Option<i64>,
) {
    let node = record(write, &labels, value);
    let id = write.create_node(node.clone()).unwrap();
    assert_eq!(
        id.0,
        model.create(labels, value),
        "IDs are handed out in order, never reused"
    );
}

impl<T: WriteTransaction> GraphTx<T> {
    /// Makes the next node ID at least `next`, as if the IDs before it were created and deleted.
    pub(super) fn skip_node_ids_to(&mut self, next: u64) {
        let current = self.next_counter(keys::NEXT_NODE_ID_KEY).unwrap();
        let held = self
            .counters
            .iter_mut()
            .find(|(key, _)| key == keys::NEXT_NODE_ID_KEY)
            .unwrap();
        held.1 = next.max(current);
    }
}

pub(super) fn names(
    read: &GraphTx<impl ReadTransaction>,
    universe: &[&'static str],
) -> BTreeMap<Token<Label>, &'static str> {
    universe
        .iter()
        .filter_map(|name| Some((read.lookup_token::<Label>(name).unwrap()?, *name)))
        .collect()
}

pub(super) fn token_names(
    names: &BTreeMap<Token<Label>, &'static str>,
    labels: &[Token<Label>],
) -> Labels {
    labels
        .iter()
        .map(|label| {
            *names
                .get(label)
                .unwrap_or_else(|| panic!("unknown label {label:?}"))
        })
        .collect()
}

pub(super) fn ids_without_a_node_worth_asking(model: &Model) -> BTreeSet<u64> {
    let mut probes: BTreeSet<u64> = (0..=model.next / GROUP + 1)
        .flat_map(|group| {
            let start = group * GROUP;
            [start.saturating_sub(1), start, start + 1]
        })
        .collect();
    for id in model.nodes.keys() {
        probes.extend([id.saturating_sub(1), id + 1]);
    }
    probes.extend([model.next, model.next + 1]);
    probes
}

/// Reads the graph through every read path and fails unless all of them agree with `model`.
pub(super) fn check(
    read: &GraphTx<impl ReadTransaction>,
    model: &Model,
) -> Result<(), TestCaseError> {
    check_with(read, model, &LABELS, VALUE)
}

/// Like [`check`], with the labels the graph may hold and the property the model's values are.
pub(super) fn check_with(
    read: &GraphTx<impl ReadTransaction>,
    model: &Model,
    universe: &[&'static str],
    value_key: &str,
) -> Result<(), TestCaseError> {
    let ids: Vec<u64> = read
        .node_ids()
        .unwrap()
        .into_iter()
        .map(|id| id.0)
        .collect();
    let expected: Vec<u64> = model.nodes.keys().copied().collect();
    prop_assert_eq!(&ids, &expected, "node IDs");
    let key = read.lookup_token::<PropertyKey>(value_key).unwrap();
    let mut reader = LabelReader::default();
    let names = names(read, universe);
    for (id, (labels, value)) in &model.nodes {
        let node = NodeId(*id);
        let stored = read.node_labels(node).unwrap();
        prop_assert!(stored.is_some(), "node {} has labels", id);
        let stored = stored.unwrap_or_default();
        prop_assert_eq!(&token_names(&names, &stored), labels, "labels of {}", id);
        let cached = read
            .with_labels(&mut reader, node, |found| found.to_vec())
            .unwrap();
        prop_assert_eq!(cached.as_deref(), Some(&*stored), "cached labels of {}", id);
        let found = read
            .node(node)
            .unwrap()
            .map(|record| key.and_then(|key| record.properties.get(&key).cloned()));
        prop_assert_eq!(
            found,
            Some(value.map(PropertyValue::Integer)),
            "value of {}",
            id
        );
    }
    for probe in ids_without_a_node_worth_asking(model) {
        if model.nodes.contains_key(&probe) {
            continue;
        }
        let node = NodeId(probe);
        prop_assert_eq!(
            read.node_label_set(node).unwrap(),
            None,
            "no node {}",
            probe
        );
        prop_assert!(read.node(node).unwrap().is_none(), "no record {}", probe);
        let cached = read.with_labels(&mut reader, node, |_| ()).unwrap();
        prop_assert_eq!(cached, None, "no cached node {}", probe);
    }
    for name in universe.iter().copied() {
        let carrying: Vec<u64> = model
            .nodes
            .iter()
            .filter(|(_, (labels, _))| labels.contains(name))
            .map(|(id, _)| *id)
            .collect();
        let Some(label) = read.lookup_token::<Label>(name).unwrap() else {
            prop_assert!(carrying.is_empty(), "{} has no token but nodes", name);
            continue;
        };
        let scanned: Vec<u64> = read
            .node_ids_with_label(label)
            .unwrap()
            .into_iter()
            .map(|id| id.0)
            .collect();
        prop_assert_eq!(&scanned, &carrying, "scan of {}", name);
        prop_assert_eq!(
            read.count_nodes_with_label(label).unwrap(),
            carrying.len() as u64,
            "count of {}",
            name
        );
    }
    let violations = read.check_integrity().unwrap();
    prop_assert!(violations.is_empty(), "integrity: {:?}", violations);
    Ok(())
}

fn check_label_sets(
    read: &GraphTx<impl ReadTransaction>,
    model: &Model,
) -> Result<(), TestCaseError> {
    let stored = read.transaction.scan_prefix(Table::LabelSets, &[]).unwrap();
    let names = names(read, &LABELS);
    let mut sets = BTreeSet::new();
    for (_, value) in &stored {
        let labels = meta::parse_label_set(value).unwrap();
        prop_assert!(
            sets.insert(token_names(&names, &labels)),
            "a label set is stored twice"
        );
    }
    prop_assert_eq!(&sets, &model.label_sets, "stored label sets");
    Ok(())
}

#[derive(Clone, Debug)]
enum Op {
    Create { labels: u8, value: Option<i8> },
    CreateMany { count: u16, labels: u8 },
    Delete(usize),
    DeleteEvery(usize),
    AddLabel(usize, usize),
    RemoveLabel(usize, usize),
    SetValue(usize, i8),
    SkipTo { group: u64, before: u64 },
    Check,
    Commit,
    Rollback,
    Reopen,
    Crash(u64),
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        6 => (any::<u8>(), any::<Option<i8>>()).prop_map(|(labels, value)| Op::Create { labels, value }),
        1 => (1u16..700, any::<u8>()).prop_map(|(count, labels)| Op::CreateMany { count, labels }),
        3 => any::<usize>().prop_map(Op::Delete),
        1 => (2usize..6).prop_map(Op::DeleteEvery),
        3 => (any::<usize>(), 0..LABELS.len()).prop_map(|(node, label)| Op::AddLabel(node, label)),
        3 => (any::<usize>(), 0..LABELS.len()).prop_map(|(node, label)| Op::RemoveLabel(node, label)),
        2 => (any::<usize>(), any::<i8>()).prop_map(|(node, value)| Op::SetValue(node, value)),
        1 => (1u64..4, 0u64..3).prop_map(|(group, before)| Op::SkipTo { group, before }),
        2 => Just(Op::Check),
        6 => Just(Op::Commit),
        2 => Just(Op::Rollback),
        1 => Just(Op::Reopen),
        1 => any::<u64>().prop_map(Op::Crash),
    ]
}

fn apply(
    write: &mut GraphTx<impl WriteTransaction>,
    model: &mut Model,
    op: &Op,
) -> Result<(), TestCaseError> {
    match *op {
        Op::Create { labels, value } => {
            create(write, model, labels_of(labels), value.map(i64::from))
        }
        Op::CreateMany { count, labels } => {
            for offset in 0..u64::from(count) {
                let mask = labels.rotate_left((offset % 8) as u32) & (offset as u8 | 0x21);
                create(write, model, labels_of(mask), Some(offset as i64));
            }
        }
        Op::Delete(index) => {
            if let Some(id) = model.pick(index) {
                prop_assert!(write.delete_node(NodeId(id)).unwrap().is_some());
                model.nodes.remove(&id);
            }
        }
        Op::DeleteEvery(stride) => {
            let doomed: Vec<u64> = model.nodes.keys().copied().step_by(stride).collect();
            for id in doomed {
                prop_assert!(write.delete_node(NodeId(id)).unwrap().is_some());
                model.nodes.remove(&id);
            }
        }
        Op::AddLabel(index, label) => {
            if let Some(id) = model.pick(index) {
                let token = write.intern_token::<Label>(LABELS[label]).unwrap();
                write.add_label(NodeId(id), token).unwrap();
                model.relabel(id, |labels| {
                    labels.insert(LABELS[label]);
                });
            }
        }
        Op::RemoveLabel(index, label) => {
            if let Some(id) = model.pick(index) {
                let token = write.intern_token::<Label>(LABELS[label]).unwrap();
                write.remove_label(NodeId(id), token).unwrap();
                model.relabel(id, |labels| {
                    labels.remove(LABELS[label]);
                });
            }
        }
        Op::SetValue(index, value) => {
            if let Some(id) = model.pick(index) {
                let key = write.intern_token::<PropertyKey>(VALUE).unwrap();
                write
                    .set_node_property(NodeId(id), key, PropertyValue::Integer(value.into()))
                    .unwrap();
                model.nodes.get_mut(&id).unwrap().1 = Some(value.into());
            }
        }
        Op::SkipTo { group, before } => {
            let next = (group * GROUP).saturating_sub(before);
            write.skip_node_ids_to(next);
            model.next = model.next.max(next);
        }
        Op::Check => check(write, model)?,
        Op::Commit | Op::Rollback | Op::Reopen | Op::Crash(_) => {}
    }
    Ok(())
}

pub(super) fn open(fs: &SimFs) -> Graph<RedbStorage> {
    Graph::open(sim_backend::storage(fs).unwrap()).unwrap()
}

fn run(ops: &[Op]) -> Result<(), TestCaseError> {
    let mut fs = SimFs::new();
    let mut graph = open(&fs);
    let mut committed = Model::default();
    let mut rest = ops.iter();
    loop {
        let mut pending = committed.clone();
        let mut write = graph.write().unwrap();
        let mut end = None;
        for op in rest.by_ref() {
            match op {
                Op::Commit | Op::Rollback | Op::Reopen | Op::Crash(_) => {
                    end = Some(op.clone());
                    break;
                }
                op => apply(&mut write, &mut pending, op)?,
            }
        }
        let done = end.is_none();
        match end.unwrap_or(Op::Commit) {
            Op::Commit => {
                write.commit().unwrap();
                pending.commit();
                committed = pending;
            }
            Op::Rollback => write.rollback().unwrap(),
            Op::Reopen => {
                drop(write);
                drop(graph);
                graph = open(&fs);
            }
            Op::Crash(seed) => {
                drop(write);
                drop(graph);
                fs = fs.crash(Crash::Reorder { seed });
                graph = open(&fs);
            }
            _ => unreachable!(),
        }
        let read = graph.read().unwrap();
        check(&read, &committed)?;
        check_label_sets(&read, &committed)?;
        if done {
            return Ok(());
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(test_scale::cases(48)))]

    #[test]
    fn node_meta_matches_a_model_through_commits_rollbacks_reopens_and_crashes(
        ops in prop::collection::vec(op(), 1..80),
    ) {
        run(&ops)?;
    }
}

fn single_commits(rounds: usize, seed: u64) -> Result<(), TestCaseError> {
    let fs = SimFs::new();
    let graph = open(&fs);
    let mut model = Model::default();
    let mut random = seed;
    let mut next = || {
        random = random
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        random >> 33
    };
    for round in 0..rounds {
        let mut write = graph.write().unwrap();
        if round == 0 {
            write.skip_node_ids_to(GROUP - 40);
            model.next = GROUP - 40;
        }
        let pick = next() as usize;
        match next() % 5 {
            0 | 1 => create(&mut write, &mut model, labels_of(next() as u8), None),
            2 => apply(&mut write, &mut model, &Op::Delete(pick))?,
            3 => apply(
                &mut write,
                &mut model,
                &Op::AddLabel(pick, next() as usize % LABELS.len()),
            )?,
            _ => apply(
                &mut write,
                &mut model,
                &Op::RemoveLabel(pick, next() as usize % LABELS.len()),
            )?,
        }
        write.commit().unwrap();
        model.commit();
        let read = graph.read().unwrap();
        check(&read, &model)?;
    }
    drop(graph);
    let graph = open(&fs);
    check(&graph.read().unwrap(), &model)?;
    check_label_sets(&graph.read().unwrap(), &model)
}

#[test]
fn many_one_node_commits_across_a_group_edge_read_back_after_each() {
    single_commits(test_scale::cases(300).min(5000) as usize, 7).unwrap();
}

#[test]
fn nodes_at_the_edges_of_groups_read_back() {
    let fs = SimFs::new();
    let graph = open(&fs);
    let mut model = Model::default();
    let mut write = graph.write().unwrap();
    for (start, count) in [(GROUP - 1, 3u8), (2 * GROUP - 1, 2), (3 * GROUP, 1)] {
        write.skip_node_ids_to(start);
        model.next = start;
        for offset in 0..count {
            create(
                &mut write,
                &mut model,
                labels_of(1 << offset),
                Some(i64::from(offset)),
            );
        }
    }
    check(&write, &model).unwrap();
    write.commit().unwrap();
    check(&graph.read().unwrap(), &model).unwrap();
    let expected: Vec<u64> = vec![4095, 4096, 4097, 8191, 8192, 12288];
    assert_eq!(model.nodes.keys().copied().collect::<Vec<_>>(), expected);
    let mut write = graph.write().unwrap();
    for id in [4096, 4097, 8191] {
        write.delete_node(NodeId(id)).unwrap().unwrap();
        model.nodes.remove(&id);
    }
    write.commit().unwrap();
    check(&graph.read().unwrap(), &model).unwrap();
    let read = graph.read().unwrap();
    let groups: Vec<u64> = read
        .transaction
        .scan_prefix(Table::NodeChunks, &[])
        .unwrap()
        .iter()
        .map(|(key, _)| u64::from_be_bytes(key[..8].try_into().unwrap()))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    assert_eq!(
        groups,
        vec![0, 2, 3],
        "a group without live nodes keeps nothing"
    );
}

#[test]
fn a_group_filled_then_emptied_answers_like_one_never_used() {
    let fs = SimFs::new();
    let graph = open(&fs);
    let mut model = Model::default();
    let mut write = graph.write().unwrap();
    for slot in 0..2 * GROUP + 5 {
        create(&mut write, &mut model, labels_of((slot % 64) as u8), None);
    }
    write.commit().unwrap();
    let mut write = graph.write().unwrap();
    for id in GROUP..2 * GROUP {
        write.delete_node(NodeId(id)).unwrap().unwrap();
        model.nodes.remove(&id);
    }
    check(&write, &model).unwrap();
    write.commit().unwrap();
    check(&graph.read().unwrap(), &model).unwrap();
    drop(graph);
    check(&open(&fs).read().unwrap(), &model).unwrap();
}

#[test]
fn a_group_with_more_than_256_label_sets_reads_back() {
    let fs = SimFs::new();
    let graph = open(&fs);
    let mut write = graph.write().unwrap();
    let names: Vec<String> = (0..9).map(|bit| format!("L{bit}")).collect();
    let tokens: Vec<Token<Label>> = names
        .iter()
        .map(|name| write.intern_token::<Label>(name).unwrap())
        .collect();
    let mut expected = Vec::new();
    for slot in 0..700u64 {
        let labels: BTreeSet<Token<Label>> = tokens
            .iter()
            .enumerate()
            .filter(|(bit, _)| slot & (1 << bit) != 0)
            .map(|(_, token)| *token)
            .collect();
        let node = NodeRecord {
            labels: labels.clone(),
            properties: Properties::new(),
        };
        assert_eq!(write.create_node(node.clone()).unwrap(), NodeId(slot));
        expected.push(labels);
    }
    write.commit().unwrap();
    drop(graph);
    let graph = open(&fs);
    let read = graph.read().unwrap();
    let mut reader = LabelReader::default();
    for (slot, labels) in expected.iter().enumerate() {
        let found = read
            .with_labels(&mut reader, NodeId(slot as u64), |found| found.to_vec())
            .unwrap()
            .unwrap();
        assert_eq!(found, labels.iter().copied().collect::<Vec<_>>());
    }
    for token in &tokens {
        let carrying = expected
            .iter()
            .filter(|labels| labels.contains(token))
            .count();
        assert_eq!(
            read.count_nodes_with_label(*token).unwrap(),
            carrying as u64
        );
        assert_eq!(read.node_ids_with_label(*token).unwrap().len(), carrying);
    }
    assert!(read.check_integrity().unwrap().is_empty());
}

fn version_of(read: &GraphTx<impl ReadTransaction>) -> usize {
    let key = read.lookup_token::<PropertyKey>(VALUE).unwrap().unwrap();
    match read.node(NodeId(0)).unwrap().unwrap().properties.get(&key) {
        Some(PropertyValue::Integer(version)) => usize::try_from(*version).unwrap(),
        other => panic!("node 0 holds no version: {other:?}"),
    }
}

#[test]
fn readers_during_writes_see_the_snapshot_they_began_at() {
    let fs = SimFs::new();
    let graph = open(&fs);
    let mut model = Model::default();
    let mut write = graph.write().unwrap();
    create(&mut write, &mut model, labels_of(1), Some(0));
    write.skip_node_ids_to(GROUP - 30);
    model.next = GROUP - 30;
    for slot in 0..40 {
        create(&mut write, &mut model, labels_of(slot), None);
    }
    write.commit().unwrap();
    let versions = Mutex::new(vec![model.clone()]);
    let commits = test_scale::cases(120) as usize;
    let done = AtomicBool::new(false);
    thread::scope(|scope| {
        let readers: Vec<_> = (0..3)
            .map(|_| {
                scope.spawn(|| {
                    let mut seen = 0;
                    while !done.load(Ordering::Acquire) {
                        let read = graph.read().unwrap();
                        let version = version_of(&read);
                        let expected = lock(&versions)[version].clone();
                        check(&read, &expected).unwrap();
                        thread::yield_now();
                        assert_eq!(version_of(&read), version);
                        check(&read, &expected).unwrap();
                        seen = seen.max(version);
                    }
                    seen
                })
            })
            .collect();
        let mut random = 11u64;
        let mut next = || {
            random = random
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (random >> 33) as usize
        };
        for version in 1..=commits {
            let mut write = graph.write().unwrap();
            let op = match next() % 6 {
                0 | 1 => Op::Create {
                    labels: next() as u8,
                    value: None,
                },
                2 => Op::CreateMany {
                    count: 50,
                    labels: next() as u8,
                },
                3 => Op::AddLabel(next(), next() % LABELS.len()),
                4 => Op::RemoveLabel(next(), next() % LABELS.len()),
                _ => Op::DeleteEvery(4),
            };
            if let Op::DeleteEvery(stride) = op {
                let doomed: Vec<u64> = model
                    .nodes
                    .keys()
                    .copied()
                    .skip(1)
                    .step_by(stride)
                    .collect();
                for id in doomed {
                    write.delete_node(NodeId(id)).unwrap().unwrap();
                    model.nodes.remove(&id);
                }
            } else {
                apply(&mut write, &mut model, &op).unwrap();
            }
            let key = write.intern_token::<PropertyKey>(VALUE).unwrap();
            write
                .set_node_property(NodeId(0), key, PropertyValue::Integer(version as i64))
                .unwrap();
            model.nodes.get_mut(&0).unwrap().1 = Some(version as i64);
            lock(&versions).push(model.clone());
            write.commit().unwrap();
        }
        done.store(true, Ordering::Release);
        for reader in readers {
            assert!(reader.join().unwrap() > 0, "a reader saw a later version");
        }
    });
}

fn stored_parts(read: &GraphTx<impl ReadTransaction>) -> (usize, usize) {
    let mut chunks = 0;
    let mut deltas = 0;
    for (key, _) in read
        .transaction
        .scan_prefix(Table::NodeChunks, &[])
        .unwrap()
    {
        match meta::parse_key(&key).unwrap().unwrap().1 {
            meta::Part::Chunk => chunks += 1,
            meta::Part::Delta(_) => deltas += 1,
        }
    }
    (chunks, deltas)
}

#[test]
fn one_node_commits_to_a_stored_group_keep_deltas_up_to_the_limit_then_rewrite_the_chunk() {
    let fs = SimFs::new();
    let graph = open(&fs);
    let mut model = Model::default();
    let mut write = graph.write().unwrap();
    for _ in 0..200 {
        create(&mut write, &mut model, labels_of(1), None);
    }
    write.commit().unwrap();
    assert_eq!(stored_parts(&graph.read().unwrap()), (1, 0));
    for round in 0..3 * meta::MAX_DELTAS {
        let mut write = graph.write().unwrap();
        apply(&mut write, &mut model, &Op::AddLabel(round, 2)).unwrap();
        write.commit().unwrap();
        let read = graph.read().unwrap();
        let expected = (round + 1) % (meta::MAX_DELTAS + 1);
        assert_eq!(stored_parts(&read), (1, expected), "after commit {round}");
        check(&read, &model).unwrap();
    }
    let mut write = graph.write().unwrap();
    apply(&mut write, &mut model, &Op::RemoveLabel(190, 2)).unwrap();
    apply(&mut write, &mut model, &Op::AddLabel(190, 2)).unwrap();
    write.commit().unwrap();
    let read = graph.read().unwrap();
    let held = (3 * meta::MAX_DELTAS) % (meta::MAX_DELTAS + 1);
    assert_eq!(
        stored_parts(&read),
        (1, held),
        "a slot changed back keeps its delta"
    );
    drop(read);
    drop(graph);
    let graph = open(&fs);
    check(&graph.read().unwrap(), &model).unwrap();
}
