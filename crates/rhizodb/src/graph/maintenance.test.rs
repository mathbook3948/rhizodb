use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;

use proptest::prelude::*;
use proptest::test_runner::TestCaseError;

use crate::graph::Direction;
use crate::graph::ElementKind;
use crate::graph::Graph;
use crate::graph::GraphTx;
use crate::graph::IndexTarget;
use crate::graph::Label;
use crate::graph::NodeRecord;
use crate::graph::Properties;
use crate::graph::PropertyKey;
use crate::graph::RelationshipId;
use crate::graph::RelationshipRecord;
use crate::graph::RelationshipType;
use crate::graph::Token;
use crate::graph::VectorMetric;
use crate::storage::ReadTransaction;
use crate::storage::WriteTransaction;
use crate::storage::redb_backend::RedbStorage;
use crate::value::PropertyValue;

const WORDS: [&str; 4] = ["alpha", "beta", "Gamma", "delta"];
const PREFIXES: [&str; 3] = ["al", "ga", "d"];
const SLOTS: usize = 4;
const LIMIT: usize = 1000;
const QUERIES: [[f64; 2]; 3] = [[1.0, 0.0], [-0.5, 1.0], [2.0, 2.0]];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Covers {
    Label(usize),
    Type(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Spec {
    Keyword { covers: Covers, both: bool },
    Vector { covers: Covers, metric: usize },
}

impl Spec {
    fn covers(self) -> Covers {
        match self {
            Self::Keyword { covers, .. } | Self::Vector { covers, .. } => covers,
        }
    }
}

fn spec(choice: usize) -> Spec {
    let covers = [
        Covers::Label(0),
        Covers::Label(1),
        Covers::Type(0),
        Covers::Type(1),
    ][choice % 4];
    if choice < 8 {
        Spec::Keyword {
            covers,
            both: choice.is_multiple_of(2),
        }
    } else {
        Spec::Vector {
            covers,
            metric: choice / 4 - 2,
        }
    }
}

fn metric(choice: usize) -> VectorMetric {
    [
        VectorMetric::Cosine,
        VectorMetric::Euclidean,
        VectorMetric::DotProduct,
    ][choice % 3]
}

fn slot_name(slot: usize) -> String {
    format!("index {slot}")
}

#[derive(Clone, Copy, Debug)]
enum On {
    Node(usize),
    Relationship(usize),
}

#[derive(Clone, Debug)]
enum Op {
    CreateNode {
        labels: [bool; 2],
        properties: Vec<(usize, PropertyValue)>,
    },
    CreateRelationship {
        from: usize,
        to: usize,
        rel_type: usize,
        properties: Vec<(usize, PropertyValue)>,
    },
    SetProperty {
        on: On,
        key: usize,
        value: PropertyValue,
    },
    RemoveProperty {
        on: On,
        key: usize,
    },
    FlipZeros {
        on: On,
        key: usize,
    },
    AddLabel {
        node: usize,
        label: usize,
    },
    RemoveLabel {
        node: usize,
        label: usize,
    },
    DeleteRelationship {
        relationship: usize,
    },
    DetachDelete {
        node: usize,
    },
    CreateIndex {
        slot: usize,
        spec: usize,
    },
    DropIndex {
        slot: usize,
    },
}

#[derive(Clone, Debug)]
enum Step {
    Transaction { ops: Vec<Op>, commit: bool },
    Reopen,
}

fn text() -> impl Strategy<Value = String> {
    prop::collection::vec(0..WORDS.len(), 1..3).prop_map(|picks| {
        picks
            .into_iter()
            .map(|pick| WORDS[pick])
            .collect::<Vec<_>>()
            .join(" ")
    })
}

fn component() -> impl Strategy<Value = PropertyValue> {
    prop_oneof![
        6 => Just(PropertyValue::Float(0.0)),
        6 => Just(PropertyValue::Float(-0.0)),
        4 => Just(PropertyValue::Float(1.0)),
        3 => Just(PropertyValue::Float(-1.0)),
        2 => Just(PropertyValue::Float(0.5)),
        3 => (-2i64..=2).prop_map(PropertyValue::Integer),
        1 => Just(PropertyValue::Float(f64::NAN)),
        1 => Just(PropertyValue::Float(f64::INFINITY)),
        1 => Just(PropertyValue::String("x".to_owned())),
    ]
}

fn value() -> impl Strategy<Value = PropertyValue> {
    prop_oneof![
        4 => text().prop_map(PropertyValue::String),
        2 => prop::collection::vec(text().prop_map(PropertyValue::String), 0..3)
            .prop_map(PropertyValue::List),
        1 => (text(), -2i64..=2).prop_map(|(text, number)| {
            PropertyValue::List(vec![PropertyValue::String(text), PropertyValue::Integer(number)])
        }),
        8 => prop::collection::vec(component(), 2).prop_map(PropertyValue::List),
        1 => prop::collection::vec(component(), 0..4).prop_map(PropertyValue::List),
        1 => (-2i64..=2).prop_map(PropertyValue::Integer),
        1 => any::<bool>().prop_map(PropertyValue::Boolean),
        1 => Just(PropertyValue::Float(-0.0)),
        1 => Just(PropertyValue::Bytes(vec![0, 1])),
    ]
}

fn properties() -> impl Strategy<Value = Vec<(usize, PropertyValue)>> {
    prop::collection::vec((0usize..3, value()), 0..3)
}

fn on() -> impl Strategy<Value = On> {
    prop_oneof![
        (0usize..8).prop_map(On::Node),
        (0usize..8).prop_map(On::Relationship),
    ]
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => (any::<[bool; 2]>(), properties())
            .prop_map(|(labels, properties)| Op::CreateNode { labels, properties }),
        3 => (0usize..8, 0usize..8, 0usize..2, properties()).prop_map(
            |(from, to, rel_type, properties)| Op::CreateRelationship {
                from,
                to,
                rel_type,
                properties,
            }
        ),
        6 => (on(), 0usize..3, value())
            .prop_map(|(on, key, value)| Op::SetProperty { on, key, value }),
        2 => (on(), 0usize..3).prop_map(|(on, key)| Op::RemoveProperty { on, key }),
        8 => (on(), Just(2usize)).prop_map(|(on, key)| Op::FlipZeros { on, key }),
        2 => (0usize..8, 0usize..2).prop_map(|(node, label)| Op::AddLabel { node, label }),
        2 => (0usize..8, 0usize..2).prop_map(|(node, label)| Op::RemoveLabel { node, label }),
        1 => (0usize..8).prop_map(|relationship| Op::DeleteRelationship { relationship }),
        1 => (0usize..8).prop_map(|node| Op::DetachDelete { node }),
        3 => (0..SLOTS, 0usize..20).prop_map(|(slot, spec)| Op::CreateIndex { slot, spec }),
        2 => (0..SLOTS).prop_map(|slot| Op::DropIndex { slot }),
    ]
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        8 => (prop::collection::vec(op(), 0..6), prop_oneof![3 => Just(true), 1 => Just(false)])
            .prop_map(|(ops, commit)| Step::Transaction { ops, commit }),
        1 => Just(Step::Reopen),
    ]
}

#[derive(Clone, Copy)]
struct Names {
    labels: [Token<Label>; 2],
    types: [Token<RelationshipType>; 2],
    keys: [Token<PropertyKey>; 3],
}

const LABEL_NAMES: [&str; 2] = ["A", "B"];
const TYPE_NAMES: [&str; 2] = ["T0", "T1"];
const KEY_NAMES: [&str; 3] = ["text", "tags", "at"];

type Model = BTreeMap<usize, Spec>;

fn relationship_ids(tx: &GraphTx<impl ReadTransaction>) -> Vec<RelationshipId> {
    let mut ids: Vec<RelationshipId> = tx
        .node_ids()
        .unwrap()
        .into_iter()
        .flat_map(|node| tx.adjacent(node, Direction::Outgoing, None).unwrap())
        .map(|adjacent| adjacent.relationship)
        .collect();
    ids.sort();
    ids
}

fn pick<T: Copy>(ids: &[T], at: usize) -> Option<T> {
    (!ids.is_empty()).then(|| ids[at % ids.len()])
}

fn property_map(names: &Names, entries: &[(usize, PropertyValue)]) -> Properties {
    entries
        .iter()
        .map(|(key, value)| (names.keys[*key], value.clone()))
        .collect()
}

fn create_index(tx: &mut GraphTx<impl WriteTransaction>, slot: usize, wanted: Spec) {
    let name = slot_name(slot);
    let target = match wanted.covers() {
        Covers::Label(label) => IndexTarget::Label(LABEL_NAMES[label]),
        Covers::Type(rel_type) => IndexTarget::RelationshipType(TYPE_NAMES[rel_type]),
    };
    match wanted {
        Spec::Keyword { both, .. } => {
            let properties: &[&str] = if both { &["text", "tags"] } else { &["text"] };
            tx.create_keyword_index(&name, target, properties).unwrap();
        }
        Spec::Vector { metric: chosen, .. } => {
            tx.create_vector_index(&name, target, "at", 2, metric(chosen))
                .unwrap();
        }
    }
}

fn flip_zeros(value: &PropertyValue) -> PropertyValue {
    match value {
        PropertyValue::Float(number) if *number == 0.0 => PropertyValue::Float(-*number),
        PropertyValue::Integer(0) => PropertyValue::Float(-0.0),
        PropertyValue::List(items) => PropertyValue::List(items.iter().map(flip_zeros).collect()),
        other => other.clone(),
    }
}

fn apply(tx: &mut GraphTx<impl WriteTransaction>, names: &Names, model: &mut Model, op: &Op) {
    match op {
        Op::CreateNode { labels, properties } => {
            let record = NodeRecord {
                labels: (0..2)
                    .filter(|label| labels[*label])
                    .map(|label| names.labels[label])
                    .collect(),
                properties: property_map(names, properties),
            };
            tx.create_node(record.clone()).unwrap();
        }
        Op::CreateRelationship {
            from,
            to,
            rel_type,
            properties,
        } => {
            let nodes = tx.node_ids().unwrap();
            if let (Some(source), Some(target)) = (pick(&nodes, *from), pick(&nodes, *to)) {
                tx.create_relationship(&RelationshipRecord {
                    rel_type: names.types[*rel_type],
                    source,
                    target,
                    properties: property_map(names, properties),
                })
                .unwrap();
            }
        }
        Op::SetProperty { on, key, value } => match on {
            On::Node(at) => {
                if let Some(id) = pick(&tx.node_ids().unwrap(), *at) {
                    tx.set_node_property(id, names.keys[*key], value.clone())
                        .unwrap();
                }
            }
            On::Relationship(at) => {
                if let Some(id) = pick(&relationship_ids(tx), *at) {
                    tx.set_relationship_property(id, names.keys[*key], value.clone())
                        .unwrap();
                }
            }
        },
        Op::RemoveProperty { on, key } => match on {
            On::Node(at) => {
                if let Some(id) = pick(&tx.node_ids().unwrap(), *at) {
                    tx.remove_node_property(id, names.keys[*key]).unwrap();
                }
            }
            On::Relationship(at) => {
                if let Some(id) = pick(&relationship_ids(tx), *at) {
                    tx.remove_relationship_property(id, names.keys[*key])
                        .unwrap();
                }
            }
        },
        Op::FlipZeros { on, key } => {
            let key = names.keys[*key];
            match on {
                On::Node(at) => {
                    if let Some(id) = pick(&tx.node_ids().unwrap(), *at)
                        && let Some(current) = tx.node(id).unwrap().unwrap().properties.get(&key)
                    {
                        let flipped = flip_zeros(current);
                        tx.set_node_property(id, key, flipped).unwrap();
                    }
                }
                On::Relationship(at) => {
                    if let Some(id) = pick(&relationship_ids(tx), *at)
                        && let Some(current) =
                            tx.relationship(id).unwrap().unwrap().properties.get(&key)
                    {
                        let flipped = flip_zeros(current);
                        tx.set_relationship_property(id, key, flipped).unwrap();
                    }
                }
            }
        }
        Op::AddLabel { node, label } => {
            if let Some(id) = pick(&tx.node_ids().unwrap(), *node) {
                tx.add_label(id, names.labels[*label]).unwrap();
            }
        }
        Op::RemoveLabel { node, label } => {
            if let Some(id) = pick(&tx.node_ids().unwrap(), *node) {
                tx.remove_label(id, names.labels[*label]).unwrap();
            }
        }
        Op::DeleteRelationship { relationship } => {
            if let Some(id) = pick(&relationship_ids(tx), *relationship) {
                tx.delete_relationship(id).unwrap();
            }
        }
        Op::DetachDelete { node } => {
            if let Some(id) = pick(&tx.node_ids().unwrap(), *node) {
                for adjacent in tx.adjacent(id, Direction::Both, None).unwrap() {
                    tx.delete_relationship(adjacent.relationship).unwrap();
                }
                tx.delete_node(id).unwrap();
            }
        }
        Op::CreateIndex { slot, spec: chosen } => {
            let wanted = spec(*chosen);
            if model.get(slot).is_some_and(|held| *held != wanted) {
                assert!(tx.drop_index(&slot_name(*slot)).unwrap());
            }
            create_index(tx, *slot, wanted);
            model.insert(*slot, wanted);
        }
        Op::DropIndex { slot } => {
            let dropped = tx.drop_index(&slot_name(*slot)).unwrap();
            assert_eq!(dropped, model.remove(slot).is_some());
        }
    }
}

struct Live {
    nodes: BTreeMap<u64, NodeRecord>,
    relationships: BTreeMap<u64, RelationshipRecord>,
}

fn live(tx: &GraphTx<impl ReadTransaction>) -> Live {
    let nodes = tx
        .node_ids()
        .unwrap()
        .into_iter()
        .map(|id| (id.0, tx.node(id).unwrap().unwrap()))
        .collect();
    let relationships = relationship_ids(tx)
        .into_iter()
        .map(|id| (id.0, tx.relationship(id).unwrap().unwrap()))
        .collect();
    Live {
        nodes,
        relationships,
    }
}

fn words_of(values: &Properties, keys: &[Token<PropertyKey>]) -> BTreeSet<String> {
    let mut words = BTreeSet::new();
    let mut add = |text: &str| {
        words.extend(text.split_whitespace().map(str::to_lowercase));
    };
    for key in keys {
        match values.get(key) {
            Some(PropertyValue::String(text)) => add(text),
            Some(PropertyValue::List(items)) => {
                for item in items {
                    if let PropertyValue::String(text) = item {
                        add(text);
                    }
                }
            }
            _ => {}
        }
    }
    words
}

fn covered<'a>(names: &Names, covers: Covers, live: &'a Live) -> Vec<(u64, &'a Properties)> {
    match covers {
        Covers::Label(label) => live
            .nodes
            .iter()
            .filter(|(_, record)| record.labels.contains(&names.labels[label]))
            .map(|(id, record)| (*id, &record.properties))
            .collect(),
        Covers::Type(rel_type) => live
            .relationships
            .iter()
            .filter(|(_, record)| record.rel_type == names.types[rel_type])
            .map(|(id, record)| (*id, &record.properties))
            .collect(),
    }
}

fn expected_vector(
    values: &Properties,
    key: Token<PropertyKey>,
    chosen: usize,
) -> Option<Vec<f64>> {
    let Some(PropertyValue::List(items)) = values.get(&key) else {
        return None;
    };
    if items.len() != 2 {
        return None;
    }
    let vector = items
        .iter()
        .map(|item| match item {
            PropertyValue::Integer(number) => Some(*number as f64),
            PropertyValue::Float(number) if number.is_finite() => Some(*number),
            _ => None,
        })
        .collect::<Option<Vec<f64>>>()?;
    let zero = vector.iter().all(|component| *component == 0.0);
    (metric(chosen) != VectorMetric::Cosine || !zero).then_some(vector)
}

fn expected_score(chosen: usize, query: &[f64], stored: &[f64]) -> f64 {
    let dot: f64 = query.iter().zip(stored).map(|(a, b)| a * b).sum();
    match metric(chosen) {
        VectorMetric::DotProduct => dot,
        VectorMetric::Euclidean => -query
            .iter()
            .zip(stored)
            .map(|(a, b)| (a - b) * (a - b))
            .sum::<f64>()
            .sqrt(),
        VectorMetric::Cosine => {
            let norm = |vector: &[f64]| vector.iter().map(|x| x * x).sum::<f64>().sqrt();
            dot / (norm(query) * norm(stored))
        }
    }
}

fn element_kind(covers: Covers) -> ElementKind {
    match covers {
        Covers::Label(_) => ElementKind::Node,
        Covers::Type(_) => ElementKind::Relationship,
    }
}

fn verify(
    tx: &GraphTx<impl ReadTransaction>,
    names: &Names,
    model: &Model,
) -> Result<String, TestCaseError> {
    let violations = tx.check_integrity().unwrap();
    prop_assert!(violations.is_empty(), "{violations:?}");
    let stored: Vec<String> = tx
        .indexes()
        .unwrap()
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    let expected: Vec<String> = model.keys().map(|slot| slot_name(*slot)).collect();
    prop_assert_eq!(&stored, &expected);

    let live = live(tx);
    let mut observed = format!("{:?}|{:?}", live.nodes, live.relationships);
    for (slot, held) in model {
        let name = slot_name(*slot);
        let kind = element_kind(held.covers());
        let elements = covered(names, held.covers(), &live);
        match *held {
            Spec::Keyword { both, .. } => {
                let keys: &[Token<PropertyKey>] = if both {
                    &names.keys[..2]
                } else {
                    &names.keys[..1]
                };
                let queries = WORDS
                    .iter()
                    .map(|word| (word.to_lowercase(), word.to_lowercase()))
                    .chain(
                        PREFIXES
                            .iter()
                            .map(|prefix| (format!("{prefix}*"), (*prefix).to_owned())),
                    );
                for (query, stem) in queries {
                    let found: BTreeSet<u64> = tx
                        .keyword_search(&name, kind, &query, LIMIT)
                        .unwrap()
                        .into_iter()
                        .map(|(id, _)| id)
                        .collect();
                    let matching: BTreeSet<u64> = elements
                        .iter()
                        .filter(|(_, values)| {
                            words_of(values, keys).iter().any(|word| {
                                if query.ends_with('*') {
                                    word.starts_with(&stem)
                                } else {
                                    *word == stem
                                }
                            })
                        })
                        .map(|(id, _)| *id)
                        .collect();
                    prop_assert_eq!(&found, &matching, "{} {:?}", name, query);
                    observed.push_str(&format!("|{name} {query} {found:?}"));
                }
            }
            Spec::Vector { metric: chosen, .. } => {
                let held: BTreeMap<u64, Vec<f64>> = elements
                    .iter()
                    .filter_map(|(id, values)| {
                        Some((*id, expected_vector(values, names.keys[2], chosen)?))
                    })
                    .collect();
                let mut queries: Vec<Vec<f64>> =
                    QUERIES.iter().map(|query| query.to_vec()).collect();
                queries.extend(held.values().cloned());
                for query in queries {
                    if metric(chosen) == VectorMetric::Cosine
                        && query.iter().all(|component| *component == 0.0)
                    {
                        continue;
                    }
                    let found = tx.vector_search(&name, kind, &query, LIMIT).unwrap();
                    let ids: BTreeSet<u64> = found.iter().map(|(id, _)| *id).collect();
                    prop_assert_eq!(&ids, &held.keys().copied().collect::<BTreeSet<u64>>());
                    for (id, score) in &found {
                        let want = expected_score(chosen, &query, &held[id]);
                        prop_assert!(
                            (score - want).abs() <= 1e-9,
                            "{name} {query:?} element {id}: {score} not {want}"
                        );
                    }
                    let exact: BTreeSet<u64> = held
                        .iter()
                        .filter(|(_, vector)| **vector == query)
                        .map(|(id, _)| *id)
                        .collect();
                    if metric(chosen) == VectorMetric::Euclidean {
                        let nearest: BTreeSet<u64> = found
                            .iter()
                            .filter(|(_, score)| *score == 0.0)
                            .map(|(id, _)| *id)
                            .collect();
                        prop_assert_eq!(&nearest, &exact);
                    }
                    observed.push_str(&format!("|{name} {query:?} {found:?}"));
                }
            }
        }
    }
    Ok(observed)
}

fn open(path: &Path) -> Graph<RedbStorage> {
    Graph::open(RedbStorage::open(path).unwrap()).unwrap()
}

fn names_of(graph: &Graph<RedbStorage>) -> Names {
    let mut tx = graph.write().unwrap();
    let names = Names {
        labels: LABEL_NAMES.map(|name| tx.intern_token(name).unwrap()),
        types: TYPE_NAMES.map(|name| tx.intern_token(name).unwrap()),
        keys: KEY_NAMES.map(|name| tx.intern_token(name).unwrap()),
    };
    tx.commit().unwrap();
    names
}

fn run(steps: &[Step]) -> Result<(), TestCaseError> {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("maintenance.redb");
    let mut graph = open(&path);
    let names = names_of(&graph);
    let mut model = Model::new();
    let mut settled = verify(&graph.read().unwrap(), &names, &model)?;
    for step in steps {
        match step {
            Step::Reopen => {
                drop(graph);
                graph = open(&path);
                let reopened = verify(&graph.read().unwrap(), &names, &model)?;
                prop_assert_eq!(&reopened, &settled);
            }
            Step::Transaction { ops, commit } => {
                let mut tx = graph.write().unwrap();
                let mut tentative = model.clone();
                for op in ops {
                    apply(&mut tx, &names, &mut tentative, op);
                }
                let inside = verify(&tx, &names, &tentative)?;
                if *commit {
                    tx.commit().unwrap();
                    model = tentative;
                    settled = verify(&graph.read().unwrap(), &names, &model)?;
                    prop_assert_eq!(&settled, &inside);
                } else {
                    tx.rollback().unwrap();
                    let after = verify(&graph.read().unwrap(), &names, &model)?;
                    prop_assert_eq!(&after, &settled);
                }
            }
        }
    }
    drop(graph);
    let graph = open(&path);
    let reopened = verify(&graph.read().unwrap(), &names, &model)?;
    prop_assert_eq!(&reopened, &settled);
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn indexes_stay_exact_across_writes_rollbacks_drops_and_reopens(
        initial in prop::collection::vec(0usize..20, SLOTS),
        steps in prop::collection::vec(step(), 1..10),
    ) {
        let ops = initial
            .into_iter()
            .enumerate()
            .map(|(slot, spec)| Op::CreateIndex { slot, spec })
            .collect();
        let mut all = vec![Step::Transaction { ops, commit: true }];
        all.extend(steps);
        run(&all)?;
    }
}
