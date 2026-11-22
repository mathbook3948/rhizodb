use proptest::prelude::*;

use super::*;
use crate::graph::Graph;
use crate::graph::records::Properties;
use crate::storage::redb_backend::RedbStorage;

const TEXTS: [&str; 6] = [
    "graph store",
    "store store tables",
    "서울에서 만나요",
    "東京都",
    "!!!",
    "Mixed CASE words",
];

#[derive(Clone, Debug)]
enum Op {
    Create { labelled: bool, text: usize },
    SetText { node: usize, text: usize },
    SetTags { node: usize, text: usize },
    RemoveText { node: usize },
    SetVector { node: usize, vector: Vec<i8> },
    AddLabel { node: usize },
    RemoveLabel { node: usize },
    Delete { node: usize },
    Rebuild,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => (any::<bool>(), 0..TEXTS.len()).prop_map(|(labelled, text)| Op::Create { labelled, text }),
        1 => (0usize..8, 0..TEXTS.len()).prop_map(|(node, text)| Op::SetText { node, text }),
        1 => (0usize..8, 0..TEXTS.len()).prop_map(|(node, text)| Op::SetTags { node, text }),
        1 => (0usize..8).prop_map(|node| Op::RemoveText { node }),
        1 => (0usize..8, prop::collection::vec(-2i8..=2, 1..4))
            .prop_map(|(node, vector)| Op::SetVector { node, vector }),
        1 => (0usize..8).prop_map(|node| Op::AddLabel { node }),
        1 => (0usize..8).prop_map(|node| Op::RemoveLabel { node }),
        1 => (0usize..8).prop_map(|node| Op::Delete { node }),
        1 => Just(Op::Rebuild),
    ]
}

struct Names {
    label: Token<Label>,
    text: Token<PropertyKey>,
    tags: Token<PropertyKey>,
    at: Token<PropertyKey>,
}

fn create_indexes(write: &mut GraphTx<impl WriteTransaction>) {
    write
        .create_keyword_index("words", "Doc", &["text", "tags"])
        .unwrap();
    write
        .create_vector_index("points", "Doc", "at", 2, VectorMetric::Cosine)
        .unwrap();
}

fn apply(write: &mut GraphTx<impl WriteTransaction>, names: &Names, op: &Op) {
    let pick = |write: &GraphTx<_>, node: usize| {
        let ids = write.node_ids().unwrap();
        (!ids.is_empty()).then(|| ids[node % ids.len()])
    };
    let text = |text: usize| PropertyValue::String(TEXTS[text].to_owned());
    match op {
        Op::Create {
            labelled,
            text: chosen,
        } => {
            let mut record = NodeRecord {
                labels: Default::default(),
                properties: Properties::from([(names.text, text(*chosen))]),
            };
            if *labelled {
                record.labels.insert(names.label);
            }
            write.create_node(&record).unwrap();
        }
        Op::SetText { node, text: chosen } => {
            if let Some(id) = pick(write, *node) {
                write
                    .set_node_property(id, names.text, text(*chosen))
                    .unwrap();
            }
        }
        Op::SetTags { node, text: chosen } => {
            if let Some(id) = pick(write, *node) {
                let tags = PropertyValue::List(vec![text(*chosen), PropertyValue::Integer(1)]);
                write.set_node_property(id, names.tags, tags).unwrap();
            }
        }
        Op::RemoveText { node } => {
            if let Some(id) = pick(write, *node) {
                write.remove_node_property(id, names.text).unwrap();
            }
        }
        Op::SetVector { node, vector } => {
            if let Some(id) = pick(write, *node) {
                let items = vector
                    .iter()
                    .map(|item| PropertyValue::Float(f64::from(*item)))
                    .collect();
                write
                    .set_node_property(id, names.at, PropertyValue::List(items))
                    .unwrap();
            }
        }
        Op::AddLabel { node } => {
            if let Some(id) = pick(write, *node) {
                write.add_label(id, names.label).unwrap();
            }
        }
        Op::RemoveLabel { node } => {
            if let Some(id) = pick(write, *node) {
                write.remove_label(id, names.label).unwrap();
            }
        }
        Op::Delete { node } => {
            if let Some(id) = pick(write, *node) {
                write.delete_node(id).unwrap();
            }
        }
        Op::Rebuild => {
            write.drop_index("words").unwrap();
            write.drop_index("points").unwrap();
            create_indexes(write);
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn indexes_match_the_data_after_any_committed_or_rolled_back_writes(
        steps in prop::collection::vec((op(), any::<bool>()), 0..60),
        indexed_after in 0usize..10,
    ) {
        let graph = Graph::open(RedbStorage::in_memory().unwrap()).unwrap();
        let names = {
            let mut write = graph.write().unwrap();
            let names = Names {
                label: write.intern_token("Doc").unwrap(),
                text: write.intern_token("text").unwrap(),
                tags: write.intern_token("tags").unwrap(),
                at: write.intern_token("at").unwrap(),
            };
            write.commit().unwrap();
            names
        };
        for (position, (op, commit)) in steps.iter().enumerate() {
            let mut write = graph.write().unwrap();
            if position == indexed_after {
                create_indexes(&mut write);
                write.commit().unwrap();
                write = graph.write().unwrap();
            }
            apply(&mut write, &names, op);
            if *commit {
                write.commit().unwrap();
            } else {
                write.rollback().unwrap();
            }
        }
        let violations = graph.read().unwrap().check_integrity().unwrap();
        prop_assert!(violations.is_empty(), "{violations:?}");
    }
}

#[test]
fn a_definition_reads_back_as_it_was_written() {
    for definition in [
        IndexDefinition {
            label: Token::new(3),
            kind: IndexKind::Keyword {
                properties: vec![Token::new(1), Token::new(4)],
            },
        },
        IndexDefinition {
            label: Token::new(0),
            kind: IndexKind::Vector {
                property: Token::new(9),
                dimension: 384,
                metric: VectorMetric::DotProduct,
            },
        },
    ] {
        let index = Index { id: 7, definition };
        assert_eq!(Index::decode(&index.encode()).unwrap(), index);
    }
}
