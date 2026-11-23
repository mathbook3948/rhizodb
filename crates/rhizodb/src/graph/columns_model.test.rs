use std::collections::BTreeMap;
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
use crate::value::tests::same_bits;

/// The property keys of the model. Indexes cover `a` (property), `c` (keyword), and `d`
/// (vector) on nodes labelled [`INDEXED`].
pub(super) const KEYS: [&str; 4] = ["a", "b", "c", "d"];
pub(super) const INDEXED: &str = "N";
const GROUP: u64 = meta::SLOTS as u64;

pub(super) type Values = BTreeMap<&'static str, PropertyValue>;

/// The nodes of a graph as the tests see them: whether each carries [`INDEXED`], and its
/// properties.
#[derive(Clone, Debug, Default)]
pub(super) struct Model {
    pub(super) nodes: BTreeMap<u64, (bool, Values)>,
    pub(super) next: u64,
}

impl Model {
    fn pick(&self, index: usize) -> Option<u64> {
        let len = self.nodes.len();
        (len > 0).then(|| *self.nodes.keys().nth(index % len).unwrap_or(&0))
    }
}

/// A value of a small domain, so writes often repeat a value, change only its type, or set a
/// float to the other zero.
pub(super) fn value_at(pick: u8) -> PropertyValue {
    match pick % 16 {
        0 => PropertyValue::Integer(0),
        1 => PropertyValue::Integer(-3),
        2 => PropertyValue::Integer(i64::MAX),
        3 => PropertyValue::Float(0.0),
        4 => PropertyValue::Float(-0.0),
        5 => PropertyValue::Float(f64::NAN),
        6 => PropertyValue::Float(1.5),
        7 => PropertyValue::Boolean(true),
        8 => PropertyValue::Boolean(false),
        9 => PropertyValue::String(String::new()),
        10 => PropertyValue::String("graph words".to_owned()),
        11 => PropertyValue::String("x".to_owned()),
        12 => PropertyValue::List(vec![PropertyValue::Float(0.5), PropertyValue::Float(1.0)]),
        13 => PropertyValue::List(vec![PropertyValue::Float(-0.0), PropertyValue::Float(2.0)]),
        14 => PropertyValue::List(vec![
            PropertyValue::Integer(1),
            PropertyValue::String("a".to_owned()),
        ]),
        _ => PropertyValue::Bytes(vec![0, 255]),
    }
}

pub(super) fn values_of(seed: u64) -> Values {
    let mut values = Values::new();
    for (at, key) in KEYS.iter().enumerate() {
        let pick = (seed >> (8 * at)) as u8;
        if !pick.is_multiple_of(5) {
            values.insert(*key, value_at(pick / 5));
        }
    }
    values
}

pub(super) fn open(fs: &SimFs) -> Graph<RedbStorage> {
    let graph = Graph::open(sim_backend::storage(fs).unwrap()).unwrap();
    let missing = graph
        .read()
        .unwrap()
        .index_definitions()
        .unwrap()
        .is_empty();
    if missing {
        let mut write = graph.write().unwrap();
        write
            .create_property_index("a", IndexTarget::Label(INDEXED), "a")
            .unwrap();
        write
            .create_keyword_index("c", IndexTarget::Label(INDEXED), &["c"])
            .unwrap();
        write
            .create_vector_index(
                "d",
                IndexTarget::Label(INDEXED),
                "d",
                2,
                VectorMetric::Cosine,
            )
            .unwrap();
        for key in KEYS {
            write.intern_token::<PropertyKey>(key).unwrap();
        }
        write.commit().unwrap();
    }
    graph
}

fn key(write: &mut GraphTx<impl WriteTransaction>, name: &str) -> Token<PropertyKey> {
    write.intern_token::<PropertyKey>(name).unwrap()
}

pub(super) fn create(
    write: &mut GraphTx<impl WriteTransaction>,
    model: &mut Model,
    indexed: bool,
    values: Values,
) {
    let mut record = NodeRecord::default();
    if indexed {
        record
            .labels
            .insert(write.intern_token::<Label>(INDEXED).unwrap());
    }
    for (name, value) in &values {
        record.properties.insert(key(write, name), value.clone());
    }
    let id = write.create_node(&record).unwrap();
    assert_eq!(id.0, model.next, "IDs are handed out in order");
    model.next += 1;
    model.nodes.insert(id.0, (indexed, values));
}

pub(super) fn set(
    write: &mut GraphTx<impl WriteTransaction>,
    model: &mut Model,
    id: u64,
    name: &'static str,
    value: Option<PropertyValue>,
) {
    let token = key(write, name);
    let values = &mut model.nodes.get_mut(&id).unwrap().1;
    match value {
        Some(value) => {
            write
                .set_node_property(NodeId(id), token, value.clone())
                .unwrap();
            values.insert(name, value);
        }
        None => {
            write.remove_node_property(NodeId(id), token).unwrap();
            values.remove(name);
        }
    }
}

pub(super) fn delete(write: &mut GraphTx<impl WriteTransaction>, model: &mut Model, id: u64) {
    let record = write.delete_node(NodeId(id)).unwrap().unwrap();
    let (_, values) = model.nodes.remove(&id).unwrap();
    assert_eq!(record.properties.len(), values.len(), "deleted {id}");
}

fn relabel(write: &mut GraphTx<impl WriteTransaction>, model: &mut Model, id: u64) {
    let label = write.intern_token::<Label>(INDEXED).unwrap();
    let indexed = &mut model.nodes.get_mut(&id).unwrap().0;
    if *indexed {
        write.remove_label(NodeId(id), label).unwrap();
    } else {
        write.add_label(NodeId(id), label).unwrap();
    }
    *indexed = !*indexed;
}

fn agrees(found: Option<&PropertyValue>, expected: Option<&PropertyValue>) -> bool {
    match (found, expected) {
        (Some(found), Some(expected)) => same_bits(found, expected),
        (None, None) => true,
        _ => false,
    }
}

fn probes(model: &Model) -> BTreeSet<u64> {
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
    probes.retain(|id| !model.nodes.contains_key(id));
    probes
}

/// Reads the graph through every node read path and fails unless all agree with `model`, and
/// unless the integrity check, which checks every index entry against the properties, passes.
pub(super) fn check(
    read: &GraphTx<impl ReadTransaction>,
    model: &Model,
) -> Result<(), TestCaseError> {
    let ids: Vec<u64> = read
        .node_ids()
        .unwrap()
        .into_iter()
        .map(|id| id.0)
        .collect();
    prop_assert_eq!(&ids, &model.nodes.keys().copied().collect::<Vec<_>>());
    let tokens: Vec<(&str, Option<Token<PropertyKey>>)> = KEYS
        .iter()
        .map(|name| (*name, read.lookup_token::<PropertyKey>(name).unwrap()))
        .collect();
    let mut labels = LabelReader::default();
    let mut columns = ColumnReader::default();
    for (id, (_, values)) in &model.nodes {
        let node = NodeId(*id);
        let record = read.node(node).unwrap();
        prop_assert!(record.is_some(), "node {}", id);
        let record = record.unwrap_or_default();
        prop_assert_eq!(
            record.properties.len(),
            values.len(),
            "properties of {}",
            id
        );
        let all = read
            .node_properties(&mut labels, &mut columns, node)
            .unwrap()
            .unwrap_or_default();
        prop_assert_eq!(all.len(), values.len(), "listed properties of {}", id);
        for (name, token) in &tokens {
            let expected = values.get(name);
            let held = token.and_then(|token| record.properties.get(&token));
            prop_assert!(agrees(held, expected), "{}.{}: {:?}", id, name, held);
            let listed = token.and_then(|token| all.get(&token));
            prop_assert!(agrees(listed, expected), "listed {}.{}", id, name);
            let shared = read
                .node_property(&mut labels, &mut columns, node, *token)
                .unwrap();
            prop_assert!(shared.is_some(), "{} exists", id);
            prop_assert!(
                agrees(shared.flatten().as_ref(), expected),
                "cached {}.{}",
                id,
                name
            );
            let fresh = read
                .node_property(
                    &mut LabelReader::default(),
                    &mut ColumnReader::default(),
                    node,
                    *token,
                )
                .unwrap();
            prop_assert!(
                agrees(fresh.flatten().as_ref(), expected),
                "probed {}.{}",
                id,
                name
            );
        }
    }
    for probe in probes(model) {
        let node = NodeId(probe);
        prop_assert!(read.node(node).unwrap().is_none(), "no node {}", probe);
        for (_, token) in &tokens {
            let found = read
                .node_property(&mut labels, &mut columns, node, *token)
                .unwrap();
            prop_assert_eq!(found, None, "no node {}", probe);
        }
        prop_assert_eq!(
            read.node_properties(&mut labels, &mut columns, node)
                .unwrap(),
            None
        );
    }
    let violations = read.check_integrity().unwrap();
    prop_assert!(violations.is_empty(), "integrity: {:?}", violations);
    Ok(())
}

#[derive(Clone, Debug)]
enum Op {
    Create { indexed: bool, seed: u64 },
    CreateMany { count: u16, seed: u64 },
    Set(usize, usize, u8),
    Remove(usize, usize),
    SetEvery { stride: usize, key: usize, pick: u8 },
    RemoveEvery { stride: usize, key: usize },
    Relabel(usize),
    Delete(usize),
    DeleteEvery(usize),
    SkipTo { group: u64, before: u64 },
    Check,
    Commit,
    Rollback,
    Reopen,
    Crash(u64),
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => (any::<bool>(), any::<u64>()).prop_map(|(indexed, seed)| Op::Create { indexed, seed }),
        1 => (1u16..600, any::<u64>()).prop_map(|(count, seed)| Op::CreateMany { count, seed }),
        8 => (any::<usize>(), 0..KEYS.len(), any::<u8>()).prop_map(|(node, key, pick)| Op::Set(node, key, pick)),
        3 => (any::<usize>(), 0..KEYS.len()).prop_map(|(node, key)| Op::Remove(node, key)),
        1 => (1usize..5, 0..KEYS.len(), any::<u8>()).prop_map(|(stride, key, pick)| Op::SetEvery { stride, key, pick }),
        1 => (1usize..5, 0..KEYS.len()).prop_map(|(stride, key)| Op::RemoveEvery { stride, key }),
        2 => any::<usize>().prop_map(Op::Relabel),
        2 => any::<usize>().prop_map(Op::Delete),
        1 => (2usize..6).prop_map(Op::DeleteEvery),
        1 => (1u64..4, 0u64..3).prop_map(|(group, before)| Op::SkipTo { group, before }),
        1 => Just(Op::Check),
        5 => Just(Op::Commit),
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
        Op::Create { indexed, seed } => create(write, model, indexed, values_of(seed)),
        Op::CreateMany { count, seed } => {
            for offset in 0..u64::from(count) {
                let mixed = seed.rotate_left((offset % 64) as u32) ^ offset;
                create(write, model, offset % 3 == 0, values_of(mixed));
            }
        }
        Op::Set(node, at, pick) => {
            if let Some(id) = model.pick(node) {
                set(write, model, id, KEYS[at], Some(value_at(pick)));
            }
        }
        Op::Remove(node, at) => {
            if let Some(id) = model.pick(node) {
                set(write, model, id, KEYS[at], None);
            }
        }
        Op::SetEvery { stride, key, pick } => {
            let chosen: Vec<u64> = model.nodes.keys().copied().step_by(stride).collect();
            for id in chosen {
                set(write, model, id, KEYS[key], Some(value_at(pick)));
            }
        }
        Op::RemoveEvery { stride, key } => {
            let chosen: Vec<u64> = model.nodes.keys().copied().step_by(stride).collect();
            for id in chosen {
                set(write, model, id, KEYS[key], None);
            }
        }
        Op::Relabel(node) => {
            if let Some(id) = model.pick(node) {
                relabel(write, model, id);
            }
        }
        Op::Delete(node) => {
            if let Some(id) = model.pick(node) {
                delete(write, model, id);
            }
        }
        Op::DeleteEvery(stride) => {
            let doomed: Vec<u64> = model.nodes.keys().copied().step_by(stride).collect();
            for id in doomed {
                delete(write, model, id);
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
    fn property_columns_match_a_model_through_commits_rollbacks_reopens_and_crashes(
        ops in prop::collection::vec(op(), 1..60),
    ) {
        run(&ops)?;
    }
}

fn stored_parts(read: &GraphTx<impl ReadTransaction>) -> BTreeMap<(u64, u32), (usize, usize)> {
    let mut parts: BTreeMap<(u64, u32), (usize, usize)> = BTreeMap::new();
    for (key, _) in read
        .transaction
        .scan_prefix(Table::NodeColumns, &[])
        .unwrap()
    {
        let (group, token, part) = columns::parse_key(&key).unwrap();
        let entry = parts.entry((group, token.id())).or_default();
        match part {
            columns::Part::Chunk => entry.0 += 1,
            columns::Part::Delta(_) => entry.1 += 1,
        }
    }
    parts
}

fn graph_of(count: u64, values: impl Fn(u64) -> Values) -> (SimFs, Graph<RedbStorage>, Model) {
    let fs = SimFs::new();
    let graph = open(&fs);
    let mut model = Model::default();
    let mut write = graph.write().unwrap();
    for id in 0..count {
        create(&mut write, &mut model, id % 2 == 0, values(id));
    }
    write.commit().unwrap();
    (fs, graph, model)
}

fn only_b(value: PropertyValue) -> impl Fn(u64) -> Values {
    move |_| Values::from([("b", value.clone())])
}

#[test]
fn one_node_commits_keep_deltas_up_to_the_limit_then_rewrite_the_chunk() {
    let (fs, graph, mut model) = graph_of(300, only_b(PropertyValue::Integer(1)));
    let b = graph
        .read()
        .unwrap()
        .lookup_token::<PropertyKey>("b")
        .unwrap()
        .unwrap();
    let column = (0, b.id());
    assert_eq!(stored_parts(&graph.read().unwrap())[&column], (1, 0));
    for round in 0..3 * columns::MAX_DELTAS {
        let mut write = graph.write().unwrap();
        set(
            &mut write,
            &mut model,
            round as u64,
            "b",
            Some(PropertyValue::Integer(2)),
        );
        write.commit().unwrap();
        let read = graph.read().unwrap();
        let expected = (round + 1) % (columns::MAX_DELTAS + 1);
        assert_eq!(read.check_integrity().unwrap(), vec![]);
        assert_eq!(
            stored_parts(&read)[&column],
            (1, expected),
            "after commit {round}"
        );
    }
    check(&graph.read().unwrap(), &model).unwrap();
    let held = stored_parts(&graph.read().unwrap())[&column].1;
    let mut write = graph.write().unwrap();
    let id = 3 * columns::MAX_DELTAS as u64 - 1;
    set(
        &mut write,
        &mut model,
        id,
        "b",
        Some(PropertyValue::Integer(1)),
    );
    write.commit().unwrap();
    assert_eq!(
        stored_parts(&graph.read().unwrap())[&column],
        (1, held - 1),
        "a value set back to the chunk's drops its delta"
    );
    let mut write = graph.write().unwrap();
    set(
        &mut write,
        &mut model,
        299,
        "b",
        Some(PropertyValue::Integer(1)),
    );
    write.commit().unwrap();
    assert_eq!(
        stored_parts(&graph.read().unwrap())[&column],
        (1, held - 1),
        "a value set to the one it holds writes nothing"
    );
    drop(graph);
    check(&open(&fs).read().unwrap(), &model).unwrap();
}

#[test]
fn a_value_that_changes_only_its_type_or_the_sign_of_a_zero_is_stored_again() {
    let (fs, graph, mut model) = graph_of(10, only_b(PropertyValue::Float(0.0)));
    let changes = [
        PropertyValue::Float(-0.0),
        PropertyValue::Integer(0),
        PropertyValue::Boolean(false),
        PropertyValue::String(String::new()),
        PropertyValue::List(vec![]),
        PropertyValue::Float(0.0),
    ];
    for (round, value) in changes.into_iter().enumerate() {
        let mut write = graph.write().unwrap();
        set(&mut write, &mut model, round as u64, "b", Some(value));
        check(&write, &model).unwrap();
        write.commit().unwrap();
        check(&graph.read().unwrap(), &model).unwrap();
    }
    drop(graph);
    check(&open(&fs).read().unwrap(), &model).unwrap();
}

#[test]
fn removing_every_value_of_a_column_deletes_its_chunk_and_deltas() {
    let (fs, graph, mut model) = graph_of(200, |id| {
        Values::from([
            ("a", PropertyValue::Integer(id as i64)),
            ("b", PropertyValue::Boolean(true)),
        ])
    });
    let mut write = graph.write().unwrap();
    for id in 0..5 {
        set(&mut write, &mut model, id, "a", None);
    }
    write.commit().unwrap();
    let a = graph
        .read()
        .unwrap()
        .lookup_token::<PropertyKey>("a")
        .unwrap()
        .unwrap();
    assert_eq!(stored_parts(&graph.read().unwrap())[&(0, a.id())], (1, 5));
    let mut write = graph.write().unwrap();
    for id in 5..200 {
        set(&mut write, &mut model, id, "a", None);
    }
    check(&write, &model).unwrap();
    write.commit().unwrap();
    let parts = stored_parts(&graph.read().unwrap());
    assert!(!parts.contains_key(&(0, a.id())), "{parts:?}");
    check(&graph.read().unwrap(), &model).unwrap();
    let mut write = graph.write().unwrap();
    set(
        &mut write,
        &mut model,
        7,
        "a",
        Some(PropertyValue::Integer(7)),
    );
    write.commit().unwrap();
    assert_eq!(stored_parts(&graph.read().unwrap())[&(0, a.id())], (1, 0));
    drop(graph);
    check(&open(&fs).read().unwrap(), &model).unwrap();
}

#[test]
fn nodes_on_both_sides_of_each_group_edge_keep_their_values_in_their_own_group() {
    let fs = SimFs::new();
    let graph = open(&fs);
    let mut model = Model::default();
    let mut write = graph.write().unwrap();
    for id in 0..=2 * GROUP + 1 {
        let value = PropertyValue::Integer(id as i64);
        create(&mut write, &mut model, false, Values::from([("a", value)]));
    }
    check(&write, &model).unwrap();
    write.commit().unwrap();
    let a = graph
        .read()
        .unwrap()
        .lookup_token::<PropertyKey>("a")
        .unwrap()
        .unwrap();
    let parts = stored_parts(&graph.read().unwrap());
    assert_eq!(
        parts.keys().copied().collect::<Vec<_>>(),
        vec![(0, a.id()), (1, a.id()), (2, a.id())]
    );
    for (group, count) in [(0u64, 4096usize), (1, 4096), (2, 2)] {
        let chunk = graph
            .read()
            .unwrap()
            .transaction
            .get(Table::NodeColumns, &columns::chunk_key(group, a))
            .unwrap()
            .unwrap();
        let column = columns::Column::load(group, a, &chunk).unwrap();
        let held = (0..meta::SLOTS)
            .filter(|slot| column.cell_at(*slot).unwrap().is_some())
            .count();
        assert_eq!(held, count, "group {group}");
    }
    let mut write = graph.write().unwrap();
    for id in [GROUP - 1, GROUP, GROUP + 1, 2 * GROUP - 1, 2 * GROUP] {
        set(
            &mut write,
            &mut model,
            id,
            "a",
            Some(PropertyValue::String(id.to_string())),
        );
    }
    delete(&mut write, &mut model, 2 * GROUP + 1);
    write.commit().unwrap();
    check(&graph.read().unwrap(), &model).unwrap();
    drop(graph);
    check(&open(&fs).read().unwrap(), &model).unwrap();
}

#[test]
fn a_new_node_in_the_slot_of_no_node_starts_with_no_values() {
    let (fs, graph, mut model) =
        graph_of(100, |id| values_of(id.wrapping_mul(0x9e37_79b9_7f4a_7c15)));
    let mut write = graph.write().unwrap();
    write.skip_node_ids_to(GROUP + 10);
    model.next = GROUP + 10;
    for id in (0..100).step_by(3) {
        delete(&mut write, &mut model, id);
    }
    create(&mut write, &mut model, true, Values::new());
    check(&write, &model).unwrap();
    write.commit().unwrap();
    check(&graph.read().unwrap(), &model).unwrap();
    drop(graph);
    check(&open(&fs).read().unwrap(), &model).unwrap();
}

fn version_of(read: &GraphTx<impl ReadTransaction>, b: Token<PropertyKey>) -> usize {
    let found = read
        .node_property(
            &mut LabelReader::default(),
            &mut ColumnReader::default(),
            NodeId(0),
            Some(b),
        )
        .unwrap();
    match found {
        Some(Some(PropertyValue::Integer(version))) => usize::try_from(version).unwrap(),
        other => panic!("node 0 holds no version: {other:?}"),
    }
}

#[test]
fn readers_during_column_writes_see_the_snapshot_they_began_at() {
    let (_fs, graph, mut model) = graph_of(GROUP + 50, |id| {
        let mut values = values_of(id);
        values.insert("b", PropertyValue::Integer(0));
        values
    });
    let b = graph
        .read()
        .unwrap()
        .lookup_token::<PropertyKey>("b")
        .unwrap()
        .unwrap();
    let versions = Mutex::new(vec![model.clone()]);
    let commits = test_scale::cases(60) as usize;
    let done = AtomicBool::new(false);
    thread::scope(|scope| {
        let readers: Vec<_> = (0..3)
            .map(|_| {
                scope.spawn(|| {
                    let mut seen = 0;
                    while !done.load(Ordering::Acquire) {
                        let read = graph.read().unwrap();
                        let version = version_of(&read, b);
                        let expected = lock(&versions)[version].clone();
                        check(&read, &expected).unwrap();
                        thread::yield_now();
                        assert_eq!(version_of(&read, b), version);
                        check(&read, &expected).unwrap();
                        seen = seen.max(version);
                    }
                    seen
                })
            })
            .collect();
        let mut random = 5u64;
        let mut next = || {
            random = random
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (random >> 33) as usize
        };
        for version in 1..=commits {
            let mut write = graph.write().unwrap();
            let op = match next() % 6 {
                0 => Op::Set(next(), next() % KEYS.len(), next() as u8),
                1 => Op::SetEvery {
                    stride: 1 + next() % 3,
                    key: 0,
                    pick: next() as u8,
                },
                2 => Op::Remove(next(), 2),
                3 => Op::Create {
                    indexed: true,
                    seed: next() as u64,
                },
                4 => Op::Relabel(next()),
                _ => Op::Set(next(), 3, next() as u8),
            };
            apply(&mut write, &mut model, &op).unwrap();
            if !model.nodes.contains_key(&0) {
                panic!("node 0 keeps the version");
            }
            set(
                &mut write,
                &mut model,
                0,
                "b",
                Some(PropertyValue::Integer(version as i64)),
            );
            lock(&versions).push(model.clone());
            write.commit().unwrap();
        }
        done.store(true, Ordering::Release);
        for reader in readers {
            assert!(reader.join().unwrap() > 0, "a reader saw a later version");
        }
    });
}
