use proptest::prelude::*;

use super::*;
use crate::graph::Graph;
use crate::graph::records::Properties;
use crate::storage::Storage;
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
fn a_vector_that_changes_only_the_sign_of_a_zero_is_stored_again() {
    let graph = Graph::open(RedbStorage::in_memory().unwrap()).unwrap();
    let mut write = graph.write().unwrap();
    write
        .create_vector_index("points", "P", "at", 2, VectorMetric::Euclidean)
        .unwrap();
    let label = write.intern_token("P").unwrap();
    let at = write.intern_token("at").unwrap();
    let vector = |first: f64| {
        PropertyValue::List(vec![PropertyValue::Float(first), PropertyValue::Float(1.0)])
    };
    let node = write
        .create_node(&NodeRecord {
            labels: [label].into(),
            properties: Properties::from([(at, vector(0.0))]),
        })
        .unwrap();
    write.set_node_property(node, at, vector(-0.0)).unwrap();
    write.commit().unwrap();
    let violations = graph.read().unwrap().check_integrity().unwrap();
    assert_eq!(violations, vec![]);
}

#[test]
fn opening_a_database_from_another_tokenizer_rebuilds_its_keyword_indexes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("stale.redb");
    {
        let graph = Graph::open(RedbStorage::open(&path).unwrap()).unwrap();
        let mut write = graph.write().unwrap();
        write
            .create_keyword_index("words", "Doc", &["text"])
            .unwrap();
        write
            .create_vector_index("points", "Doc", "at", 1, VectorMetric::DotProduct)
            .unwrap();
        let label = write.intern_token("Doc").unwrap();
        let text = write.intern_token("text").unwrap();
        let at = write.intern_token("at").unwrap();
        write
            .create_node(&NodeRecord {
                labels: [label].into(),
                properties: Properties::from([
                    (text, PropertyValue::String("current terms".to_owned())),
                    (at, PropertyValue::List(vec![PropertyValue::Integer(2)])),
                ]),
            })
            .unwrap();
        write.commit().unwrap();

        let mut stale = graph.storage.begin_write().unwrap();
        for table in [Table::KeywordPostings, Table::KeywordDocuments] {
            for (key, _) in stale.scan_prefix(table, &[]).unwrap() {
                stale.delete(table, &key).unwrap();
            }
        }
        let index = Index {
            id: 0,
            definition: IndexDefinition {
                label,
                kind: IndexKind::Keyword {
                    properties: vec![text],
                },
            },
        };
        let old_output = Entry::Keyword(Document::from([("stale".to_owned(), vec![0, 1, 2])]));
        for (table, key, value) in index.rows(NodeId(0), &old_output) {
            stale.put(table, &key, &value).unwrap();
        }
        stale
            .put(
                Table::KeywordTotals,
                &keys::index_prefix(0),
                &encode_totals(1, 3),
            )
            .unwrap();
        stale
            .put(Table::Metadata, keys::TOKENIZER_KEY, b"an older tokenizer")
            .unwrap();
        stale.commit().unwrap();
        assert!(!graph.read().unwrap().check_integrity().unwrap().is_empty());
    }
    let graph = Graph::open(RedbStorage::open(&path).unwrap()).unwrap();
    let read = graph.read().unwrap();
    assert_eq!(read.check_integrity().unwrap(), vec![]);
    assert_eq!(read.keyword_search("words", "stale", 10).unwrap(), vec![]);
    assert_eq!(
        read.keyword_search("words", "current", 10).unwrap().len(),
        1
    );
    assert_eq!(read.vector_search("points", &[1.0], 10).unwrap().len(), 1);
}

#[test]
fn opening_a_database_of_the_previous_format_rebuilds_its_keyword_indexes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("previous.redb");
    {
        let graph = Graph::open(RedbStorage::open(&path).unwrap()).unwrap();
        let mut write = graph.write().unwrap();
        write
            .create_keyword_index("words", "Doc", &["text"])
            .unwrap();
        let label = write.intern_token("Doc").unwrap();
        let text = write.intern_token("text").unwrap();
        write
            .create_node(&NodeRecord {
                labels: [label].into(),
                properties: Properties::from([(
                    text,
                    PropertyValue::String("graph store graph".to_owned()),
                )]),
            })
            .unwrap();
        write.commit().unwrap();

        let mut previous = graph.storage.begin_write().unwrap();
        for (key, value) in previous.scan_prefix(Table::KeywordPostings, &[]).unwrap() {
            previous
                .put(Table::KeywordPostings, &key, &value[..8])
                .unwrap();
        }
        previous
            .put(
                Table::Metadata,
                keys::FORMAT_VERSION_KEY,
                &3u32.to_be_bytes(),
            )
            .unwrap();
        previous.commit().unwrap();
        assert!(!graph.read().unwrap().check_integrity().unwrap().is_empty());
    }
    let graph = Graph::open(RedbStorage::open(&path).unwrap()).unwrap();
    let read = graph.read().unwrap();
    assert_eq!(read.check_integrity().unwrap(), vec![]);
    assert_eq!(
        read.keyword_search("words", "\"graph store\"", 10)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn opening_records_the_current_tokenizer() {
    let graph = Graph::open(RedbStorage::in_memory().unwrap()).unwrap();
    let stored = graph
        .read()
        .unwrap()
        .transaction
        .get(Table::Metadata, keys::TOKENIZER_KEY)
        .unwrap();
    assert_eq!(stored, Some(tokenize::fingerprint()));
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
