use std::cell::Cell;
use std::rc::Rc;

use proptest::prelude::*;
use proptest::test_runner::TestCaseError;

use super::*;
use crate::engine::sim_fs::Crash;
use crate::engine::sim_fs::SimFs;
use crate::storage::test_engine;
use crate::storage::test_engine::TestStorage;
use crate::test_scale;

pub(super) const TYPES: [&str; 3] = ["A", "B", "C"];

/// The nodes and relationships a graph holds: each relationship's source, target, and the
/// position of its type in `TYPES`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Model {
    pub(super) nodes: BTreeSet<u64>,
    pub(super) rels: BTreeMap<u64, (u64, u64, usize)>,
}

pub(super) type Write<'a> = GraphTx<<TestStorage as Storage>::Write<'a>>;

pub(super) fn open(fs: &SimFs) -> Graph<TestStorage> {
    let graph = Graph::open(test_engine::on_sim(fs).unwrap()).unwrap();
    let mut write = graph.write().unwrap();
    for name in TYPES {
        write.intern_token::<RelationshipType>(name).unwrap();
    }
    write.commit().unwrap();
    graph
}

pub(super) fn rel_type(index: usize) -> Token<RelationshipType> {
    Token::new(index as u32)
}

/// Creates a node with the next ID at or after `at`.
pub(super) fn create_node(write: &mut Write<'_>, model: &mut Model, at: u64) -> u64 {
    write.skip_node_ids_to(at);
    let id = write.create_node(NodeRecord::default()).unwrap().0;
    model.nodes.insert(id);
    id
}

pub(super) fn create_rel(
    write: &mut Write<'_>,
    model: &mut Model,
    source: u64,
    target: u64,
    index: usize,
) -> u64 {
    let record = RelationshipRecord {
        rel_type: rel_type(index),
        source: NodeId(source),
        target: NodeId(target),
        properties: Properties::new(),
    };
    let id = write.create_relationship(&record).unwrap().0;
    model.rels.insert(id, (source, target, index));
    id
}

pub(super) fn delete_rel(write: &mut Write<'_>, model: &mut Model, id: u64) {
    assert!(
        write
            .delete_relationship(RelationshipId(id))
            .unwrap()
            .is_some()
    );
    model.rels.remove(&id);
}

pub(super) fn detach_delete(write: &mut Write<'_>, model: &mut Model, node: u64) {
    for adjacent in write.adjacent(NodeId(node), Direction::Both, None).unwrap() {
        delete_rel(write, model, adjacent.relationship.0);
    }
    assert!(write.delete_node(NodeId(node)).unwrap().is_some());
    model.nodes.remove(&node);
}

/// What `adjacent` returns for `node` by the model: outgoing relationships, then incoming
/// ones, each side by type and then by ID, with a self-loop once under `Both`.
pub(super) fn expected(
    model: &Model,
    node: u64,
    direction: Direction,
    only: Option<usize>,
) -> Vec<Adjacent> {
    let side = |outgoing: bool| {
        let mut found: Vec<Adjacent> = model
            .rels
            .iter()
            .filter(|(_, (source, target, index))| {
                only.is_none_or(|only| only == *index)
                    && if outgoing {
                        *source == node
                    } else {
                        *target == node && !(direction == Direction::Both && *source == node)
                    }
            })
            .map(|(id, (source, target, index))| Adjacent {
                relationship: RelationshipId(*id),
                rel_type: rel_type(*index),
                other: NodeId(if outgoing { *target } else { *source }),
            })
            .collect();
        found.sort_by_key(|adjacent| (adjacent.rel_type, adjacent.relationship));
        found
    };
    let mut found = Vec::new();
    if direction != Direction::Incoming {
        found.extend(side(true));
    }
    if direction != Direction::Outgoing {
        found.extend(side(false));
    }
    found
}

/// Checks every node's adjacency in every direction, of every type and of each one, and the
/// integrity of the stored graph.
pub(super) fn check(
    read: &GraphTx<impl ReadTransaction>,
    model: &Model,
) -> Result<(), TestCaseError> {
    check_with(
        read,
        model,
        &[Direction::Outgoing, Direction::Incoming, Direction::Both],
        &[None, Some(0), Some(1), Some(2)],
    )
}

/// Checks every node's adjacency on each side, of every type, and the integrity of the stored
/// graph.
pub(super) fn check_sides(
    read: &GraphTx<impl ReadTransaction>,
    model: &Model,
) -> Result<(), TestCaseError> {
    check_with(
        read,
        model,
        &[Direction::Outgoing, Direction::Incoming],
        &[None],
    )
}

fn check_with(
    read: &GraphTx<impl ReadTransaction>,
    model: &Model,
    directions: &[Direction],
    types: &[Option<usize>],
) -> Result<(), TestCaseError> {
    for direction in directions.iter().copied() {
        for only in types.iter().copied() {
            let mut reader = AdjacencyReader::default();
            for node in &model.nodes {
                let expected = expected(model, *node, direction, only);
                let found = read
                    .adjacent(NodeId(*node), direction, only.map(rel_type))
                    .unwrap();
                prop_assert_eq!(
                    &found,
                    &expected,
                    "node {} {:?} {:?}",
                    node,
                    direction,
                    only
                );
                let scanned = read
                    .adjacent_with(&mut reader, NodeId(*node), direction, only.map(rel_type))
                    .unwrap();
                prop_assert_eq!(
                    scanned,
                    expected,
                    "node {} {:?} {:?} in a scan",
                    node,
                    direction,
                    only
                );
            }
        }
    }
    let violations = read.check_integrity().unwrap();
    prop_assert!(violations.is_empty(), "{:?}", violations);
    Ok(())
}

#[derive(Clone, Debug)]
enum Op {
    CreateNode(u64),
    CreateRel(usize, usize, usize),
    DeleteRel(usize),
    DetachDelete(usize),
    Fan {
        center: usize,
        index: usize,
        incoming: bool,
        times: usize,
    },
    Check,
    Commit,
    Rollback,
    Reopen,
    Crash(u64),
}

const PLACES: [u64; 8] = [0, 63, 64, 4095, 4096, 4097, 8191, 12_288];

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => (0..PLACES.len()).prop_map(|at| Op::CreateNode(PLACES[at])),
        10 => (any::<usize>(), any::<usize>(), 0..TYPES.len())
            .prop_map(|(source, target, index)| Op::CreateRel(source, target, index)),
        4 => any::<usize>().prop_map(Op::DeleteRel),
        1 => any::<usize>().prop_map(Op::DetachDelete),
        1 => (any::<usize>(), 0..TYPES.len(), any::<bool>(), 1..4usize).prop_map(
            |(center, index, incoming, times)| Op::Fan { center, index, incoming, times }
        ),
        1 => Just(Op::Check),
        2 => Just(Op::Commit),
        1 => Just(Op::Rollback),
        1 => Just(Op::Reopen),
        1 => any::<u64>().prop_map(Op::Crash),
    ]
}

fn pick(set: impl ExactSizeIterator<Item = u64>, at: usize) -> Option<u64> {
    let len = set.len();
    let mut set = set;
    (len > 0).then(|| set.nth(at % len)).flatten()
}

fn apply(write: &mut Write<'_>, model: &mut Model, op: &Op) -> Result<(), TestCaseError> {
    match op {
        Op::CreateNode(at) => {
            create_node(write, model, *at);
        }
        Op::CreateRel(source, target, index) => {
            if let (Some(source), Some(target)) = (
                pick(model.nodes.iter().copied(), *source),
                pick(model.nodes.iter().copied(), *target),
            ) {
                create_rel(write, model, source, target, *index);
            }
        }
        Op::DeleteRel(at) => {
            if let Some(id) = pick(model.rels.keys().copied(), *at) {
                delete_rel(write, model, id);
            }
        }
        Op::DetachDelete(at) => {
            if let Some(node) = pick(model.nodes.iter().copied(), *at) {
                detach_delete(write, model, node);
            }
        }
        Op::Fan {
            center,
            index,
            incoming,
            times,
        } => {
            if let Some(center) = pick(model.nodes.iter().copied(), *center) {
                let others: Vec<u64> = model.nodes.iter().copied().collect();
                for _ in 0..*times {
                    for other in &others {
                        let (source, target) = if *incoming {
                            (*other, center)
                        } else {
                            (center, *other)
                        };
                        create_rel(write, model, source, target, *index);
                    }
                }
            }
        }
        Op::Check => check(write, model)?,
        Op::Commit | Op::Rollback | Op::Reopen | Op::Crash(_) => {}
    }
    Ok(())
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
        check(&graph.read().unwrap(), &committed)?;
        if done {
            return Ok(());
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(test_scale::cases(32)))]

    #[test]
    #[ignore = "left out of mise run check, run by mise run random"]
    fn adjacency_matches_a_model_through_commits_rollbacks_reopens_and_crashes(
        ops in prop::collection::vec(op(), 1..80),
    ) {
        run(&ops)?;
    }
}

#[test]
fn a_scan_of_many_consecutive_segments_reads_what_each_node_reads_alone() {
    let fs = SimFs::new();
    let graph = open(&fs);
    let mut model = Model::default();
    let mut write = graph.write().unwrap();
    let nodes: Vec<u64> = (0..1200)
        .map(|at| create_node(&mut write, &mut model, 3000 + at))
        .collect();
    for (at, node) in nodes.iter().enumerate() {
        if at % 97 < 40 {
            continue;
        }
        let other = nodes[(at * 31) % nodes.len()];
        create_rel(&mut write, &mut model, *node, other, at % 2 * 2);
        if at % 5 == 0 {
            create_rel(&mut write, &mut model, other, *node, 1);
        }
    }
    write.commit().unwrap();
    check(&graph.read().unwrap(), &model).unwrap();
    let mut write = graph.write().unwrap();
    for node in nodes.iter().step_by(23) {
        create_rel(&mut write, &mut model, *node, *node, 0);
    }
    let doomed: Vec<u64> = model.rels.keys().copied().step_by(11).collect();
    for id in doomed {
        delete_rel(&mut write, &mut model, id);
    }
    check(&write, &model).unwrap();
    write.commit().unwrap();
    check(&graph.read().unwrap(), &model).unwrap();
}

#[test]
fn a_hub_of_every_kind_reads_back_after_many_small_commits_and_one_large_one() {
    let fs = SimFs::new();
    let graph = open(&fs);
    let mut model = Model::default();
    let mut write = graph.write().unwrap();
    let hub = create_node(&mut write, &mut model, 4095);
    let leaves: Vec<u64> = (0..300)
        .map(|at| create_node(&mut write, &mut model, 4096 + at * 13))
        .collect();
    write.commit().unwrap();
    for round in 0..40 {
        let mut write = graph.write().unwrap();
        for leaf in leaves.iter().skip(round).step_by(37) {
            create_rel(&mut write, &mut model, hub, *leaf, round % 2);
            create_rel(&mut write, &mut model, *leaf, hub, 2);
        }
        if round % 5 == 4 {
            let doomed: Vec<u64> = model.rels.keys().copied().step_by(7).collect();
            for id in doomed {
                delete_rel(&mut write, &mut model, id);
            }
        }
        write.commit().unwrap();
        check(&graph.read().unwrap(), &model).unwrap();
    }
    let mut write = graph.write().unwrap();
    for _ in 0..20 {
        for leaf in &leaves {
            create_rel(&mut write, &mut model, hub, *leaf, 1);
        }
    }
    create_rel(&mut write, &mut model, hub, hub, 1);
    write.commit().unwrap();
    assert!(model.rels.len() > 6_000);
    check(&graph.read().unwrap(), &model).unwrap();
    let mut write = graph.write().unwrap();
    detach_delete(&mut write, &mut model, hub);
    check(&write, &model).unwrap();
    write.commit().unwrap();
    check(&graph.read().unwrap(), &model).unwrap();
    let read = graph.read().unwrap();
    assert!(
        read.transaction
            .scan_prefix(Table::Adjacency, &[])
            .unwrap()
            .is_empty()
    );
}

#[derive(Debug, Default)]
struct Visits {
    asked: Cell<u32>,
    walks: Cell<usize>,
    passed_apart_of_others: Cell<usize>,
    loaded_of_others: Cell<usize>,
}

impl Visits {
    fn of_others(&self, table: Table, key: &[u8]) -> Option<bool> {
        if table != Table::Adjacency {
            return None;
        }
        let (id, _) = adjacency::parse_key(key).ok()?;
        Some(id.rel_type != self.asked.get())
    }

    fn walk(&self, table: Table) {
        if table == Table::Adjacency {
            add(&self.walks, 1);
        }
    }

    fn pass(&self, table: Table, key: &[u8], value: Peek<'_>) {
        if matches!(value, Peek::Apart(_)) && self.of_others(table, key) == Some(true) {
            add(&self.passed_apart_of_others, 1);
        }
    }

    fn load(&self, table: Table, key: &[u8]) {
        if self.of_others(table, key) == Some(true) {
            add(&self.loaded_of_others, 1);
        }
    }

    fn take(&self) -> (usize, usize, usize) {
        (
            self.walks.take(),
            self.passed_apart_of_others.take(),
            self.loaded_of_others.take(),
        )
    }
}

fn add(counter: &Cell<usize>, count: usize) {
    counter.set(counter.get() + count);
}

struct CountingStorage {
    inner: TestStorage,
    visits: Rc<Visits>,
}

struct CountingRead<'a> {
    inner: <TestStorage as Storage>::Read<'a>,
    visits: Rc<Visits>,
}

impl Storage for CountingStorage {
    type Read<'a> = CountingRead<'a>;
    type Write<'a> = <TestStorage as Storage>::Write<'a>;

    fn begin_read(&self) -> Result<CountingRead<'_>, StorageError> {
        Ok(CountingRead {
            inner: self.inner.begin_read()?,
            visits: Rc::clone(&self.visits),
        })
    }

    fn begin_write(&self) -> Result<Self::Write<'_>, StorageError> {
        self.inner.begin_write()
    }

    fn write_copy(snapshot: &CountingRead<'_>, path: &std::path::Path) -> Result<(), StorageError> {
        TestStorage::write_copy(&snapshot.inner, path)
    }
}

impl ReadTransaction for CountingRead<'_> {
    const SEES_ONLY_COMMITTED: bool =
        <<TestStorage as Storage>::Read<'static> as ReadTransaction>::SEES_ONLY_COMMITTED;

    fn get(&self, table: Table, key: &[u8]) -> Result<Option<Vec<u8>>, StorageError> {
        self.visits.load(table, key);
        self.inner.get(table, key)
    }

    fn scan_prefix(
        &self,
        table: Table,
        prefix: &[u8],
    ) -> Result<Vec<crate::storage::Entry>, StorageError> {
        let entries = self.inner.scan_prefix(table, prefix)?;
        for (key, _) in &entries {
            self.visits.load(table, key);
        }
        Ok(entries)
    }

    fn scan_range(
        &self,
        table: Table,
        start: &[u8],
        end: &[u8],
    ) -> Result<Vec<crate::storage::Entry>, StorageError> {
        let entries = self.inner.scan_range(table, start, end)?;
        for (key, _) in &entries {
            self.visits.load(table, key);
        }
        Ok(entries)
    }

    fn get_with<R, E: From<StorageError>>(
        &self,
        table: Table,
        key: &[u8],
        read: impl FnOnce(&[u8]) -> Result<R, E>,
    ) -> Result<Option<R>, E> {
        self.visits.load(table, key);
        self.inner.get_with(table, key, read)
    }

    fn visit_prefix<E: From<StorageError>>(
        &self,
        table: Table,
        prefix: &[u8],
        mut visit: impl FnMut(&[u8], &[u8]) -> Result<(), E>,
    ) -> Result<(), E> {
        self.inner.visit_prefix(table, prefix, |key, value| {
            self.visits.load(table, key);
            visit(key, value)
        })
    }

    fn visit_range<E: From<StorageError>>(
        &self,
        table: Table,
        start: &[u8],
        end: &[u8],
        mut visit: impl FnMut(&[u8], &[u8]) -> Result<(), E>,
    ) -> Result<(), E> {
        self.inner.visit_range(table, start, end, |key, value| {
            self.visits.load(table, key);
            visit(key, value)
        })
    }

    fn visit_prefix_peek<E: From<StorageError>>(
        &self,
        table: Table,
        prefix: &[u8],
        mut visit: impl FnMut(&[u8], Peek<'_>) -> Result<(), E>,
    ) -> Result<(), E> {
        self.visits.walk(table);
        self.inner.visit_prefix_peek(table, prefix, |key, value| {
            self.visits.pass(table, key, value);
            visit(key, value)
        })
    }

    fn visit_range_peek<E: From<StorageError>>(
        &self,
        table: Table,
        start: &[u8],
        end: &[u8],
        mut visit: impl FnMut(&[u8], Peek<'_>) -> Result<(), E>,
    ) -> Result<(), E> {
        self.visits.walk(table);
        self.inner
            .visit_range_peek(table, start, end, |key, value| {
                self.visits.pass(table, key, value);
                visit(key, value)
            })
    }
}

#[test]
fn a_typed_scan_beside_a_heavy_type_reads_no_value_of_the_other_type() {
    let visits = Rc::new(Visits::default());
    let graph = Graph::open(CountingStorage {
        inner: test_engine::in_memory(),
        visits: Rc::clone(&visits),
    })
    .unwrap();
    let mut model = Model::default();
    let mut write = graph.write().unwrap();
    for name in TYPES {
        write.intern_token::<RelationshipType>(name).unwrap();
    }
    let nodes: Vec<u64> = (0..64)
        .map(|at| create_node(&mut write, &mut model, 1024 + at))
        .collect();
    for (at, node) in nodes.iter().enumerate() {
        create_rel(
            &mut write,
            &mut model,
            *node,
            nodes[(at + 1) % nodes.len()],
            0,
        );
        for far in nodes.iter().cycle().skip(at).take(600) {
            create_rel(&mut write, &mut model, *node, *far, 2);
        }
    }
    write.commit().unwrap();
    graph.storage.inner.checkpoint().unwrap();
    let read = graph.read().unwrap();
    for (asked, other) in [(0, 2), (2, 0)] {
        for direction in [Direction::Outgoing, Direction::Both] {
            visits.asked.set(rel_type(asked).id());
            let alone: Vec<_> = nodes
                .iter()
                .map(|node| {
                    read.adjacent(NodeId(*node), direction, Some(rel_type(asked)))
                        .unwrap()
                })
                .collect();
            let (walks_alone, _, loaded_alone) = visits.take();
            let mut reader = AdjacencyReader::default();
            let scanned: Vec<_> = nodes
                .iter()
                .map(|node| {
                    read.adjacent_with(&mut reader, NodeId(*node), direction, Some(rel_type(asked)))
                        .unwrap()
                })
                .collect();
            let (walks, passed_apart, loaded) = visits.take();
            let case = format!("type {asked} {direction:?}");
            assert_eq!(scanned, alone, "{case}");
            assert_eq!(loaded_alone, 0, "{case}");
            assert_eq!(loaded, 0, "{case} read values of type {other}");
            assert!(
                walks * 8 <= walks_alone,
                "{case}: {walks} walks, {walks_alone} alone"
            );
            if asked == 0 {
                assert!(
                    passed_apart > 0,
                    "{case} passed no part of type {other} kept apart"
                );
            }
        }
    }
}
