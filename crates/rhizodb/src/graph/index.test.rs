use proptest::prelude::*;
use std::cell::Cell;
use std::cell::RefCell;
use std::rc::Rc;

use super::*;
use crate::graph::Direction;
use crate::graph::Graph;
use crate::graph::NodeId;
use crate::graph::RelationshipId;
use crate::graph::records::Properties;
use crate::storage::Storage;
use crate::storage::StorageError;
use crate::storage::test_engine;
use crate::storage::test_engine::TestStorage;

const TEXTS: [&str; 6] = [
    "graph store",
    "store store tables",
    "서울에서 만나요",
    "東京都",
    "!!!",
    "Mixed CASE words",
];

#[derive(Clone, Copy, Debug)]
enum On {
    Node(usize),
    Relationship(usize),
}

#[derive(Clone, Debug)]
enum Op {
    Create {
        labelled: bool,
        text: usize,
    },
    Link {
        from: usize,
        to: usize,
        linked: bool,
        text: usize,
    },
    SetText {
        on: On,
        text: usize,
    },
    SetTags {
        on: On,
        text: usize,
    },
    RemoveText {
        on: On,
    },
    SetVector {
        on: On,
        vector: Vec<i8>,
    },
    AddLabel {
        node: usize,
    },
    RemoveLabel {
        node: usize,
    },
    Unlink {
        relationship: usize,
    },
    DetachDelete {
        node: usize,
    },
    Rebuild,
}

fn on() -> impl Strategy<Value = On> {
    prop_oneof![
        (0usize..8).prop_map(On::Node),
        (0usize..8).prop_map(On::Relationship),
    ]
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => (any::<bool>(), 0..TEXTS.len()).prop_map(|(labelled, text)| Op::Create { labelled, text }),
        3 => (0usize..8, 0usize..8, any::<bool>(), 0..TEXTS.len())
            .prop_map(|(from, to, linked, text)| Op::Link { from, to, linked, text }),
        2 => (on(), 0..TEXTS.len()).prop_map(|(on, text)| Op::SetText { on, text }),
        1 => (on(), 0..TEXTS.len()).prop_map(|(on, text)| Op::SetTags { on, text }),
        1 => on().prop_map(|on| Op::RemoveText { on }),
        2 => (on(), prop::collection::vec(-2i8..=2, 1..4))
            .prop_map(|(on, vector)| Op::SetVector { on, vector }),
        1 => (0usize..8).prop_map(|node| Op::AddLabel { node }),
        1 => (0usize..8).prop_map(|node| Op::RemoveLabel { node }),
        1 => (0usize..8).prop_map(|relationship| Op::Unlink { relationship }),
        1 => (0usize..8).prop_map(|node| Op::DetachDelete { node }),
        1 => Just(Op::Rebuild),
    ]
}

struct Names {
    label: Token<Label>,
    link: Token<RelationshipType>,
    other: Token<RelationshipType>,
    text: Token<PropertyKey>,
    tags: Token<PropertyKey>,
    at: Token<PropertyKey>,
}

fn create_indexes(write: &mut GraphTx<impl WriteTransaction>) {
    write
        .create_keyword_index("words", IndexTarget::Label("Doc"), &["text", "tags"])
        .unwrap();
    write
        .create_vector_index(
            "points",
            IndexTarget::Label("Doc"),
            "at",
            2,
            VectorMetric::Cosine,
        )
        .unwrap();
}

fn create_relationship_indexes(write: &mut GraphTx<impl WriteTransaction>) {
    write
        .create_keyword_index(
            "link words",
            IndexTarget::RelationshipType("LINK"),
            &["text", "tags"],
        )
        .unwrap();
    write
        .create_vector_index(
            "link points",
            IndexTarget::RelationshipType("LINK"),
            "at",
            2,
            VectorMetric::Euclidean,
        )
        .unwrap();
}

fn create_property_indexes(write: &mut GraphTx<impl WriteTransaction>) {
    write
        .create_property_index("doc text", IndexTarget::Label("Doc"), "text")
        .unwrap();
    write
        .create_property_index("doc at", IndexTarget::Label("Doc"), "at")
        .unwrap();
    write
        .create_property_index("link text", IndexTarget::RelationshipType("LINK"), "text")
        .unwrap();
}

fn relationship_ids(write: &GraphTx<impl ReadTransaction>) -> Vec<RelationshipId> {
    let mut ids: Vec<RelationshipId> = write
        .node_ids()
        .unwrap()
        .into_iter()
        .flat_map(|node| write.adjacent(node, Direction::Outgoing, None).unwrap())
        .map(|adjacent| adjacent.relationship)
        .collect();
    ids.sort();
    ids
}

fn set(
    write: &mut GraphTx<impl WriteTransaction>,
    on: On,
    key: Token<PropertyKey>,
    value: Option<PropertyValue>,
) {
    let pick = |ids: Vec<u64>, at: usize| (!ids.is_empty()).then(|| ids[at % ids.len()]);
    match on {
        On::Node(at) => {
            let ids = write
                .node_ids()
                .unwrap()
                .into_iter()
                .map(|id| id.0)
                .collect();
            if let Some(id) = pick(ids, at).map(NodeId) {
                match value {
                    Some(value) => write.set_node_property(id, key, value).unwrap(),
                    None => write.remove_node_property(id, key).unwrap(),
                }
            }
        }
        On::Relationship(at) => {
            let ids = relationship_ids(write).into_iter().map(|id| id.0).collect();
            if let Some(id) = pick(ids, at).map(RelationshipId) {
                match value {
                    Some(value) => write.set_relationship_property(id, key, value).unwrap(),
                    None => write.remove_relationship_property(id, key).unwrap(),
                }
            }
        }
    }
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
            write.create_node(record.clone()).unwrap();
        }
        Op::Link {
            from,
            to,
            linked,
            text: chosen,
        } => {
            if let (Some(source), Some(target)) = (pick(write, *from), pick(write, *to)) {
                let record = RelationshipRecord {
                    rel_type: if *linked { names.link } else { names.other },
                    source,
                    target,
                    properties: Properties::from([(names.text, text(*chosen))]),
                };
                write.create_relationship(&record).unwrap();
            }
        }
        Op::SetText { on, text: chosen } => set(write, *on, names.text, Some(text(*chosen))),
        Op::SetTags { on, text: chosen } => {
            let tags = PropertyValue::List(vec![text(*chosen), PropertyValue::Integer(1)]);
            set(write, *on, names.tags, Some(tags));
        }
        Op::RemoveText { on } => set(write, *on, names.text, None),
        Op::SetVector { on, vector } => {
            let items = vector
                .iter()
                .map(|item| PropertyValue::Float(f64::from(*item)))
                .collect();
            set(write, *on, names.at, Some(PropertyValue::List(items)));
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
        Op::Unlink { relationship } => {
            let ids = relationship_ids(write);
            if !ids.is_empty() {
                write
                    .delete_relationship(ids[relationship % ids.len()])
                    .unwrap();
            }
        }
        Op::DetachDelete { node } => {
            if let Some(id) = pick(write, *node) {
                for adjacent in write.adjacent(id, Direction::Both, None).unwrap() {
                    write.delete_relationship(adjacent.relationship).unwrap();
                }
                write.delete_node(id).unwrap();
            }
        }
        Op::Rebuild => {
            for name in [
                "words",
                "points",
                "link words",
                "link points",
                "doc text",
                "doc at",
                "link text",
            ] {
                write.drop_index(name).unwrap();
            }
            create_indexes(write);
            create_relationship_indexes(write);
            create_property_indexes(write);
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    #[ignore = "left out of mise run check, run by mise run random"]
    fn indexes_match_the_data_after_any_committed_or_rolled_back_writes(
        steps in prop::collection::vec((op(), any::<bool>()), 0..60),
        indexed_after in 0usize..10,
    ) {
        let graph = Graph::open(test_engine::in_memory()).unwrap();
        let names = {
            let mut write = graph.write().unwrap();
            let names = Names {
                label: write.intern_token("Doc").unwrap(),
                link: write.intern_token("LINK").unwrap(),
                other: write.intern_token("OTHER").unwrap(),
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
                create_relationship_indexes(&mut write);
                create_property_indexes(&mut write);
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
    let graph = Graph::open(test_engine::in_memory()).unwrap();
    let mut write = graph.write().unwrap();
    write
        .create_vector_index(
            "points",
            IndexTarget::Label("P"),
            "at",
            2,
            VectorMetric::Euclidean,
        )
        .unwrap();
    let label = write.intern_token("P").unwrap();
    let at = write.intern_token("at").unwrap();
    let vector = |first: f64| {
        PropertyValue::List(vec![PropertyValue::Float(first), PropertyValue::Float(1.0)])
    };
    let node = write
        .create_node(NodeRecord {
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
    let path = directory.path().join("stale.rhizodb");
    {
        let graph = Graph::open(test_engine::open(&path)).unwrap();
        let mut write = graph.write().unwrap();
        write
            .create_keyword_index("words", IndexTarget::Label("Doc"), &["text"])
            .unwrap();
        write
            .create_vector_index(
                "points",
                IndexTarget::Label("Doc"),
                "at",
                1,
                VectorMetric::DotProduct,
            )
            .unwrap();
        let label = write.intern_token("Doc").unwrap();
        let text = write.intern_token("text").unwrap();
        let at = write.intern_token("at").unwrap();
        write
            .create_node(NodeRecord {
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
                target: Target::Label(label),
                kind: IndexKind::Keyword {
                    properties: vec![text],
                },
            },
        };
        let old_output = Entry::Keyword(Document::from([("stale".to_owned(), vec![0, 1, 2])]));
        for (table, key, value) in index.rows(0, &old_output) {
            stale.put(table, &key, &value).unwrap();
        }
        let mut block = Block::default();
        block.insert(0, 0, &[0, 1, 2], 3).unwrap();
        stale
            .put(
                Table::KeywordPostings,
                &keys::posting_key(0, "stale", 0),
                block.bytes().unwrap(),
            )
            .unwrap();
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
    let graph = Graph::open(test_engine::open(&path)).unwrap();
    let read = graph.read().unwrap();
    assert_eq!(read.check_integrity().unwrap(), vec![]);
    assert_eq!(
        read.keyword_search("words", ElementKind::Node, "stale", 10)
            .unwrap(),
        vec![]
    );
    assert_eq!(
        read.keyword_search("words", ElementKind::Node, "current", 10)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        read.vector_search("points", ElementKind::Node, &[1.0], 10)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn a_rebuild_cursor_of_the_clear_from_its_start_decodes_and_an_unknown_phase_is_refused() {
    let mut cursor = 7u32.to_be_bytes().to_vec();
    cursor.extend_from_slice(&[0; 9]);
    assert_eq!(Rebuild::decode(&cursor).unwrap(), (7, Rebuild::start(7)));
    for rebuild in [
        Rebuild::start(7),
        Rebuild::Clearing {
            table: 4,
            from: keys::index_entry_key(7, 9).to_vec(),
        },
        Rebuild::Filling { at: 9 },
    ] {
        assert_eq!(Rebuild::decode(&rebuild.encode(7)).unwrap(), (7, rebuild));
    }
    let outside = Rebuild::Clearing {
        table: 0,
        from: keys::index_prefix(8).to_vec(),
    };
    let beyond = Rebuild::Clearing {
        table: 5,
        from: keys::index_prefix(7).to_vec(),
    };
    let mut from_an_element = 7u32.to_be_bytes().to_vec();
    from_an_element.push(0);
    from_an_element.extend_from_slice(&9u64.to_be_bytes());
    let mut unknown = 7u32.to_be_bytes().to_vec();
    unknown.push(3);
    for cursor in [
        outside.encode(7),
        beyond.encode(7),
        from_an_element,
        unknown,
    ] {
        assert!(
            matches!(Rebuild::decode(&cursor), Err(GraphError::Corrupt(_))),
            "{cursor:?}"
        );
    }
}

#[test]
fn a_rebuild_a_build_before_the_resumable_clear_left_midway_restarts_its_clear() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("midway.rhizodb");
    {
        let graph = Graph::open(test_engine::open(&path)).unwrap();
        let mut write = graph.write().unwrap();
        write
            .create_keyword_index("words", IndexTarget::Label("Doc"), &["text"])
            .unwrap();
        let label = write.intern_token("Doc").unwrap();
        let text = write.intern_token("text").unwrap();
        for k in 0..50 {
            write
                .create_node(NodeRecord {
                    labels: [label].into(),
                    properties: Properties::from([(
                        text,
                        PropertyValue::String(format!("river t{k}")),
                    )]),
                })
                .unwrap();
        }
        write.commit().unwrap();

        let mut midway = graph.storage.begin_write().unwrap();
        let rows = midway
            .scan_prefix(Table::KeywordPostings, &keys::index_prefix(0))
            .unwrap();
        for (key, _) in &rows[..rows.len() / 2] {
            midway.delete(Table::KeywordPostings, key).unwrap();
        }
        midway
            .put(Table::Metadata, keys::TOKENIZER_KEY, b"an older tokenizer")
            .unwrap();
        midway
            .put(
                Table::Metadata,
                keys::FORMAT_VERSION_KEY,
                &(super::super::FORMAT_VERSION | super::super::UPGRADING).to_be_bytes(),
            )
            .unwrap();
        let mut progress = super::super::FORMAT_VERSION.to_be_bytes().to_vec();
        progress.extend_from_slice(&[super::super::upgrade::UpgradeStep::Tokenizer.code(), 1]);
        progress.extend_from_slice(&0u32.to_be_bytes());
        progress.extend_from_slice(&[0; 9]);
        midway
            .put(Table::Metadata, keys::UPGRADE_PROGRESS_KEY, &progress)
            .unwrap();
        midway.commit().unwrap();
    }
    let graph = Graph::open(test_engine::open(&path)).unwrap();
    let read = graph.read().unwrap();
    assert_eq!(read.check_integrity().unwrap(), vec![]);
    assert_eq!(
        read.keyword_search("words", ElementKind::Node, "river", 100)
            .unwrap()
            .len(),
        50
    );
}

#[test]
fn indexes_created_and_dropped_in_a_transaction_are_seen_by_its_later_reads_and_writes() {
    let graph = Graph::open(test_engine::in_memory()).unwrap();
    let mut write = graph.write().unwrap();
    let label = write.intern_token("Doc").unwrap();
    let text = write.intern_token("text").unwrap();
    let doc = || NodeRecord {
        labels: [label].into(),
        properties: Properties::from([(text, PropertyValue::String("a".to_owned()))]),
    };
    write.create_node(doc()).unwrap();
    let target = Target::Label(label);
    assert_eq!(write.property_index(target, text).unwrap(), None);
    write
        .create_property_index("doc text", IndexTarget::Label("Doc"), "text")
        .unwrap();
    let index = write.property_index(target, text).unwrap().unwrap();
    write.create_node(doc()).unwrap();
    let a = KeyRange::equal_to(&PropertyValue::String("a".to_owned())).unwrap();
    assert_eq!(write.property_seek(index, &[a]).unwrap().len(), 2);
    assert!(write.drop_index("doc text").unwrap());
    assert_eq!(write.property_index(target, text).unwrap(), None);
    write.create_node(doc()).unwrap();
    write.commit().unwrap();
    assert_eq!(graph.read().unwrap().check_integrity().unwrap(), vec![]);
}

#[test]
fn later_transactions_see_a_committed_index_and_not_a_rolled_back_one() {
    let graph = Graph::open(test_engine::in_memory()).unwrap();
    let mut write = graph.write().unwrap();
    let label = write.intern_token::<Label>("Doc").unwrap();
    let text = write.intern_token::<PropertyKey>("text").unwrap();
    write.commit().unwrap();
    let target = Target::Label(label);
    let mut write = graph.write().unwrap();
    write
        .create_property_index("doc text", IndexTarget::Label("Doc"), "text")
        .unwrap();
    write.rollback().unwrap();
    assert_eq!(
        graph.read().unwrap().property_index(target, text).unwrap(),
        None
    );
    let mut write = graph.write().unwrap();
    write
        .create_property_index("doc text", IndexTarget::Label("Doc"), "text")
        .unwrap();
    write.commit().unwrap();
    assert!(
        graph
            .read()
            .unwrap()
            .property_index(target, text)
            .unwrap()
            .is_some()
    );
    assert!(
        graph
            .write()
            .unwrap()
            .property_index(target, text)
            .unwrap()
            .is_some()
    );
}

#[test]
fn opening_a_database_recorded_under_older_unicode_tables_records_the_current_tokenizer() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("unicode.rhizodb");
    let mut older = tokenize::unicode_versions();
    older[0].0 -= 1;
    {
        let graph = Graph::open(test_engine::open(&path)).unwrap();
        let mut write = graph.write().unwrap();
        write
            .create_keyword_index("words", IndexTarget::Label("Doc"), &["text"])
            .unwrap();
        let label = write.intern_token("Doc").unwrap();
        let text = write.intern_token("text").unwrap();
        write
            .create_node(NodeRecord {
                labels: [label].into(),
                properties: Properties::from([(
                    text,
                    PropertyValue::String("current terms".to_owned()),
                )]),
            })
            .unwrap();
        write
            .transaction
            .put(
                Table::Metadata,
                keys::TOKENIZER_KEY,
                &tokenize::fingerprint_of(&older),
            )
            .unwrap();
        write.commit().unwrap();
        assert!(!graph.read().unwrap().check_integrity().unwrap().is_empty());
    }
    let graph = Graph::open(test_engine::open(&path)).unwrap();
    let read = graph.read().unwrap();
    assert_eq!(read.check_integrity().unwrap(), vec![]);
    assert_eq!(
        read.transaction
            .get(Table::Metadata, keys::TOKENIZER_KEY)
            .unwrap(),
        Some(tokenize::fingerprint())
    );
    assert_eq!(
        read.keyword_search("words", ElementKind::Node, "current", 10)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn opening_records_the_current_tokenizer() {
    let graph = Graph::open(test_engine::in_memory()).unwrap();
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
            target: Target::Label(Token::new(3)),
            kind: IndexKind::Keyword {
                properties: vec![Token::new(1), Token::new(4)],
            },
        },
        IndexDefinition {
            target: Target::Label(Token::new(0)),
            kind: IndexKind::Vector {
                property: Token::new(9),
                dimension: 384,
                metric: VectorMetric::DotProduct,
            },
        },
        IndexDefinition {
            target: Target::RelationshipType(Token::new(3)),
            kind: IndexKind::Keyword {
                properties: vec![Token::new(2)],
            },
        },
        IndexDefinition {
            target: Target::RelationshipType(Token::new(5)),
            kind: IndexKind::Vector {
                property: Token::new(1),
                dimension: 2,
                metric: VectorMetric::Euclidean,
            },
        },
    ] {
        let index = Index { id: 7, definition };
        assert_eq!(Index::decode(&index.encode()).unwrap(), index);
    }
    let over_label = Index {
        id: 7,
        definition: IndexDefinition {
            target: Target::Label(Token::new(3)),
            kind: IndexKind::Keyword {
                properties: vec![Token::new(2)],
            },
        },
    };
    let over_type = Index {
        id: 7,
        definition: IndexDefinition {
            target: Target::RelationshipType(Token::new(3)),
            ..over_label.definition.clone()
        },
    };
    assert_ne!(over_label.encode(), over_type.encode());
}

fn row_posting(positions: &[u32], length: u32) -> Vec<u8> {
    let mut out = u32::try_from(positions.len())
        .unwrap()
        .to_be_bytes()
        .to_vec();
    out.extend_from_slice(&length.to_be_bytes());
    let mut previous = 0;
    for position in positions {
        put_varint(&mut out, usize::try_from(position - previous).unwrap());
        previous = *position;
    }
    out
}

#[test]
fn a_posting_row_of_format_thirteen_reads_its_positions_and_length() {
    let posting = row_posting(&[0, 5, 9], 12);
    assert_eq!(decode_row_posting(&posting).unwrap(), (vec![0, 5, 9], 12));
}

#[test]
fn a_posting_row_cut_short_or_left_long_fails_to_decode() {
    let posting = row_posting(&[0, 5, 9], 12);
    for end in 0..posting.len() {
        assert!(decode_row_posting(&posting[..end]).is_err(), "{end}");
    }
    let mut long = posting.clone();
    long.push(1);
    assert!(decode_row_posting(&long).is_err());
    assert!(decode_row_posting(&row_posting(&[], 0)).is_err());
}

#[test]
fn a_stored_vector_reads_only_at_its_index_dimension() {
    let bytes: Vec<u8> = [1.5f64, -2.0]
        .iter()
        .flat_map(|item| item.to_be_bytes())
        .collect();
    let numbers: Vec<f64> = stored_vector(&bytes, 2)
        .unwrap()
        .iter()
        .map(|item| f64::from_be_bytes(*item))
        .collect();
    assert_eq!(numbers, [1.5, -2.0]);
    assert!(matches!(
        stored_vector(&bytes, 3),
        Err(DecodeError::UnexpectedEnd)
    ));
    assert!(matches!(
        stored_vector(&bytes[..15], 2),
        Err(DecodeError::UnexpectedEnd)
    ));
    assert!(matches!(
        stored_vector(&bytes, 1),
        Err(DecodeError::TrailingBytes)
    ));
    assert!(matches!(
        stored_vector(&bytes[..12], 1),
        Err(DecodeError::TrailingBytes)
    ));
}

#[test]
fn creating_a_relationship_index_fills_it_from_relationships_of_its_type_alone() {
    let graph = Graph::open(test_engine::in_memory()).unwrap();
    let mut write = graph.write().unwrap();
    let link = write.intern_token("LINK").unwrap();
    let other = write.intern_token("OTHER").unwrap();
    let text = write.intern_token("text").unwrap();
    let mut node = || {
        write
            .create_node(NodeRecord {
                labels: Default::default(),
                properties: Properties::new(),
            })
            .unwrap()
    };
    let first = node();
    let second = node();
    let mut linked = Vec::new();
    for (rel_type, source, target) in [
        (link, first, second),
        (other, first, second),
        (link, second, second),
        (link, second, first),
        (other, first, first),
    ] {
        let id = write
            .create_relationship(&RelationshipRecord {
                rel_type,
                source,
                target,
                properties: Properties::from([(
                    text,
                    PropertyValue::String("shared words".to_owned()),
                )]),
            })
            .unwrap();
        if rel_type == link {
            linked.push(id.0);
        }
    }
    create_relationship_indexes(&mut write);
    let mut found: Vec<u64> = write
        .keyword_search("link words", ElementKind::Relationship, "shared", 10)
        .unwrap()
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    found.sort();
    assert_eq!(found, linked);
    write.commit().unwrap();
    let violations = graph.read().unwrap().check_integrity().unwrap();
    assert_eq!(violations, vec![]);
}

#[test]
fn writes_after_creating_or_dropping_an_index_in_the_same_transaction_follow_it() {
    let graph = Graph::open(test_engine::in_memory()).unwrap();
    let mut write = graph.write().unwrap();
    let label = write.intern_token("Doc").unwrap();
    let link = write.intern_token("LINK").unwrap();
    let text = write.intern_token("text").unwrap();
    let words = |write: &GraphTx<_>, element| {
        write
            .keyword_search("words", element, "shared", 10)
            .unwrap()
            .len()
    };
    let doc = NodeRecord {
        labels: [label].into(),
        properties: Properties::from([(text, PropertyValue::String("shared".to_owned()))]),
    };
    let first = write.create_node(doc.clone()).unwrap();
    let relationship = |source| RelationshipRecord {
        rel_type: link,
        source,
        target: first,
        properties: Properties::from([(text, PropertyValue::String("shared".to_owned()))]),
    };
    write.create_relationship(&relationship(first)).unwrap();

    write
        .create_keyword_index("words", IndexTarget::RelationshipType("LINK"), &["text"])
        .unwrap();
    let second = write.create_node(doc.clone()).unwrap();
    let linked = write.create_relationship(&relationship(second)).unwrap();
    assert_eq!(words(&write, ElementKind::Relationship), 2);
    write.drop_index("words").unwrap();
    write
        .set_relationship_property(linked, text, PropertyValue::String("other".to_owned()))
        .unwrap();
    write.delete_relationship(linked).unwrap();

    write
        .create_keyword_index("words", IndexTarget::Label("Doc"), &["text"])
        .unwrap();
    write.create_relationship(&relationship(second)).unwrap();
    write.create_node(doc.clone()).unwrap();
    write.remove_node_property(first, text).unwrap();
    assert_eq!(words(&write, ElementKind::Node), 2);
    write.drop_index("words").unwrap();
    write.create_node(doc.clone()).unwrap();

    write
        .create_vector_index(
            "words",
            IndexTarget::RelationshipType("LINK"),
            "at",
            1,
            VectorMetric::DotProduct,
        )
        .unwrap();
    let pointed = write.create_relationship(&relationship(first)).unwrap();
    let at = write.intern_token("at").unwrap();
    write
        .set_relationship_property(
            pointed,
            at,
            PropertyValue::List(vec![PropertyValue::Integer(3)]),
        )
        .unwrap();
    let found = write
        .vector_search("words", ElementKind::Relationship, &[1.0], 10)
        .unwrap();
    assert_eq!(found, vec![(pointed.0, 3.0)]);
    write.commit().unwrap();
    let violations = graph.read().unwrap().check_integrity().unwrap();
    assert_eq!(violations, vec![]);
}

#[derive(Debug)]
struct CountingStorage {
    inner: TestStorage,
    entry_writes: Rc<Cell<usize>>,
    posting_bytes: Rc<Cell<usize>>,
    posting_gets: Rc<Cell<usize>>,
    log: Rc<RefCell<Vec<Access>>>,
}

#[derive(Debug)]
enum Access {
    Scan(Table, Vec<u8>),
    Delete(Table, Vec<u8>),
    Commit,
}

struct CountingWrite<'a> {
    inner: <TestStorage as Storage>::Write<'a>,
    entry_writes: Rc<Cell<usize>>,
    posting_bytes: Rc<Cell<usize>>,
    posting_gets: Rc<Cell<usize>>,
    log: Rc<RefCell<Vec<Access>>>,
}

impl CountingWrite<'_> {
    fn count(&self, table: Table) {
        if ENTRY_TABLES.contains(&table) && table != Table::KeywordPostings {
            self.entry_writes.set(self.entry_writes.get() + 1);
        }
    }

    fn read(&self, table: Table, value: &[u8]) {
        if table == Table::KeywordPostings {
            self.posting_bytes
                .set(self.posting_bytes.get() + value.len());
        }
    }
}

impl Storage for CountingStorage {
    type Read<'a> = <TestStorage as Storage>::Read<'a>;
    type Write<'a> = CountingWrite<'a>;

    fn begin_read(&self) -> Result<Self::Read<'_>, StorageError> {
        self.inner.begin_read()
    }

    fn begin_write(&self) -> Result<CountingWrite<'_>, StorageError> {
        Ok(CountingWrite {
            inner: self.inner.begin_write()?,
            entry_writes: Rc::clone(&self.entry_writes),
            posting_bytes: Rc::clone(&self.posting_bytes),
            posting_gets: Rc::clone(&self.posting_gets),
            log: Rc::clone(&self.log),
        })
    }

    fn write_copy(snapshot: &Self::Read<'_>, path: &std::path::Path) -> Result<(), StorageError> {
        TestStorage::write_copy(snapshot, path)
    }
}

impl ReadTransaction for CountingWrite<'_> {
    fn get(&self, table: Table, key: &[u8]) -> Result<Option<Vec<u8>>, StorageError> {
        self.inner.get(table, key)
    }

    fn scan_prefix(
        &self,
        table: Table,
        prefix: &[u8],
    ) -> Result<Vec<crate::storage::Entry>, StorageError> {
        self.inner.scan_prefix(table, prefix)
    }

    fn scan_range(
        &self,
        table: Table,
        start: &[u8],
        end: &[u8],
    ) -> Result<Vec<crate::storage::Entry>, StorageError> {
        self.inner.scan_range(table, start, end)
    }

    fn get_with<R, E: From<StorageError>>(
        &self,
        table: Table,
        key: &[u8],
        read: impl FnOnce(&[u8]) -> Result<R, E>,
    ) -> Result<Option<R>, E> {
        if table == Table::KeywordPostings {
            self.posting_gets.set(self.posting_gets.get() + 1);
        }
        self.inner.get_with(table, key, |value| {
            self.read(table, value);
            read(value)
        })
    }

    fn visit_prefix<E: From<StorageError>>(
        &self,
        table: Table,
        prefix: &[u8],
        mut visit: impl FnMut(&[u8], &[u8]) -> Result<(), E>,
    ) -> Result<(), E> {
        self.inner.visit_prefix(table, prefix, |key, value| {
            self.read(table, value);
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
            self.read(table, value);
            visit(key, value)
        })
    }

    fn visit_prefix_peek<E: From<StorageError>>(
        &self,
        table: Table,
        prefix: &[u8],
        visit: impl FnMut(&[u8], crate::storage::Peek<'_>) -> Result<(), E>,
    ) -> Result<(), E> {
        self.log
            .borrow_mut()
            .push(Access::Scan(table, prefix.to_vec()));
        self.inner.visit_prefix_peek(table, prefix, visit)
    }

    fn visit_range_peek<E: From<StorageError>>(
        &self,
        table: Table,
        start: &[u8],
        end: &[u8],
        visit: impl FnMut(&[u8], crate::storage::Peek<'_>) -> Result<(), E>,
    ) -> Result<(), E> {
        self.log
            .borrow_mut()
            .push(Access::Scan(table, start.to_vec()));
        self.inner.visit_range_peek(table, start, end, visit)
    }
}

impl WriteTransaction for CountingWrite<'_> {
    fn put(&mut self, table: Table, key: &[u8], value: &[u8]) -> Result<(), StorageError> {
        self.count(table);
        self.inner.put(table, key, value)
    }

    fn delete(&mut self, table: Table, key: &[u8]) -> Result<(), StorageError> {
        self.count(table);
        self.log
            .borrow_mut()
            .push(Access::Delete(table, key.to_vec()));
        self.inner.delete(table, key)
    }

    fn delete_range(&mut self, table: Table, start: &[u8], end: &[u8]) -> Result<(), StorageError> {
        self.count(table);
        self.inner.delete_range(table, start, end)
    }

    fn commit(self) -> Result<(), StorageError> {
        self.log.borrow_mut().push(Access::Commit);
        self.inner.commit()
    }

    fn rollback(self) -> Result<(), StorageError> {
        self.inner.rollback()
    }
}

#[test]
fn a_rebuild_clears_every_entry_without_scanning_back_over_its_committed_deletions() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("clear.rhizodb");
    let entries = {
        let graph = Graph::open(test_engine::open(&path)).unwrap();
        let mut write = graph.write().unwrap();
        write
            .create_keyword_index("words", IndexTarget::Label("Doc"), &["text"])
            .unwrap();
        let label = write.intern_token("Doc").unwrap();
        let text = write.intern_token("text").unwrap();
        for k in 0..3000 {
            let words: Vec<String> = (0..20).map(|i| format!("t{k}x{i}")).collect();
            write
                .create_node(NodeRecord {
                    labels: [label].into(),
                    properties: Properties::from([(text, PropertyValue::String(words.join(" ")))]),
                })
                .unwrap();
        }
        write.commit().unwrap();
        let mut stale = graph.storage.begin_write().unwrap();
        stale
            .put(Table::Metadata, keys::TOKENIZER_KEY, b"an older tokenizer")
            .unwrap();
        let mut entries = BTreeSet::new();
        for (at, table) in ENTRY_TABLES.into_iter().enumerate() {
            for (key, _) in stale.scan_prefix(table, &keys::index_prefix(0)).unwrap() {
                entries.insert((at, key));
            }
        }
        stale.commit().unwrap();
        entries
    };
    let log = Rc::new(RefCell::new(Vec::new()));
    let graph = Graph::open(CountingStorage {
        inner: test_engine::open(&path),
        entry_writes: Rc::default(),
        posting_bytes: Rc::default(),
        posting_gets: Rc::default(),
        log: Rc::clone(&log),
    })
    .unwrap();
    assert_eq!(graph.read().unwrap().check_integrity().unwrap(), vec![]);
    let at = |table: Table| ENTRY_TABLES.iter().position(|entry| *entry == table);
    let mut committed = BTreeSet::new();
    let mut deleting = Vec::new();
    let mut scans = 0;
    for access in log.borrow().iter() {
        match access {
            Access::Scan(table, start) => {
                let Some(table) = at(*table) else { continue };
                let walked = committed
                    .range((table, start.clone())..(table + 1, Vec::new()))
                    .count();
                assert!(walked <= 1, "a scan of {table} walks {walked} deletions");
                scans += 1;
            }
            Access::Delete(table, key) => {
                if let Some(table) = at(*table) {
                    deleting.push((table, key.clone()));
                }
            }
            Access::Commit => committed.extend(deleting.drain(..)),
        }
    }
    assert!(scans >= 10, "{scans}");
    let missed: Vec<_> = entries.difference(&committed).take(3).collect();
    assert!(missed.is_empty(), "the clear kept {missed:?}");
}

#[test]
fn dropping_an_index_deletes_its_entries_by_range_instead_of_one_by_one() {
    let directory = tempfile::tempdir().unwrap();
    let entry_writes = Rc::new(Cell::new(0));
    let graph = Graph::open(CountingStorage {
        inner: test_engine::open(directory.path().join("drop.rhizodb")),
        entry_writes: Rc::clone(&entry_writes),
        posting_bytes: Rc::default(),
        posting_gets: Rc::default(),
        log: Rc::default(),
    })
    .unwrap();
    let mut write = graph.write().unwrap();
    create_indexes(&mut write);
    let label = write.intern_token("Doc").unwrap();
    let text = write.intern_token("text").unwrap();
    let at = write.intern_token("at").unwrap();
    for i in 0..200 {
        write
            .create_node(NodeRecord {
                labels: [label].into(),
                properties: Properties::from([
                    (text, PropertyValue::String(format!("word{i} graph"))),
                    (
                        at,
                        PropertyValue::List(vec![
                            PropertyValue::Float(1.0),
                            PropertyValue::Float(f64::from(i)),
                        ]),
                    ),
                ]),
            })
            .unwrap();
    }
    write
        .create_keyword_index("more words", IndexTarget::Label("Doc"), &["text"])
        .unwrap();
    write.commit().unwrap();
    entry_writes.set(0);
    let mut write = graph.write().unwrap();
    assert!(write.drop_index("points").unwrap());
    write.commit().unwrap();
    assert!(
        entry_writes.get() <= ENTRY_TABLES.len(),
        "{} entry writes",
        entry_writes.get()
    );
    let read = graph.read().unwrap();
    assert_eq!(read.check_integrity().unwrap(), vec![]);
    assert_eq!(
        read.transaction.scan_prefix(Table::Vectors, &[]).unwrap(),
        vec![]
    );
    for neighbour in ["words", "more words"] {
        assert_eq!(
            read.keyword_search(neighbour, ElementKind::Node, "graph", 1000)
                .unwrap()
                .len(),
            200,
            "{neighbour}"
        );
    }
    drop(read);
    let mut write = graph.write().unwrap();
    assert!(write.drop_index("words").unwrap());
    assert!(write.drop_index("more words").unwrap());
    write.commit().unwrap();
    let read = graph.read().unwrap();
    assert_eq!(read.check_integrity().unwrap(), vec![]);
    for table in ENTRY_TABLES {
        assert_eq!(read.transaction.scan_prefix(table, &[]).unwrap(), vec![]);
    }
}

#[test]
fn dropping_the_index_of_the_largest_id_deletes_its_entries() {
    let graph = Graph::open(test_engine::in_memory()).unwrap();
    let mut write = graph.write().unwrap();
    write
        .transaction
        .put(
            Table::Metadata,
            keys::NEXT_INDEX_ID_KEY,
            &u64::from(u32::MAX - 1).to_be_bytes(),
        )
        .unwrap();
    for name in ["below", "words"] {
        write
            .create_keyword_index(name, IndexTarget::Label("Doc"), &["text"])
            .unwrap();
    }
    let label = write.intern_token("Doc").unwrap();
    let text = write.intern_token("text").unwrap();
    write
        .create_node(NodeRecord {
            labels: [label].into(),
            properties: Properties::from([(text, PropertyValue::String("graph".to_owned()))]),
        })
        .unwrap();
    write.commit().unwrap();
    let mut write = graph.write().unwrap();
    assert!(write.drop_index("words").unwrap());
    write.commit().unwrap();
    let read = graph.read().unwrap();
    assert_eq!(read.check_integrity().unwrap(), vec![]);
    assert_eq!(
        read.keyword_search("below", ElementKind::Node, "graph", 10)
            .unwrap()
            .len(),
        1
    );
    for table in ENTRY_TABLES {
        for (key, _) in read.transaction.scan_prefix(table, &[]).unwrap() {
            assert!(
                key.starts_with(&keys::index_prefix(u32::MAX - 1)),
                "{table:?} keeps {key:?}"
            );
        }
    }
}

#[test]
fn a_phrase_reads_a_common_word_only_where_its_rarest_word_occurs() {
    let posting_bytes = Rc::new(Cell::new(0));
    let graph = Graph::open(CountingStorage {
        inner: test_engine::in_memory(),
        entry_writes: Rc::default(),
        posting_bytes: Rc::clone(&posting_bytes),
        posting_gets: Rc::default(),
        log: Rc::default(),
    })
    .unwrap();
    let mut write = graph.write().unwrap();
    create_indexes(&mut write);
    let label = write.intern_token("Doc").unwrap();
    let text = write.intern_token("text").unwrap();
    let mut zebras = Vec::new();
    for i in 0..2000 {
        let zebra = i % 1000 == 999;
        let words = if zebra {
            "the zebra the end"
        } else {
            "the cat on the mat"
        };
        let node = write
            .create_node(NodeRecord {
                labels: [label].into(),
                properties: Properties::from([(text, PropertyValue::String(words.to_owned()))]),
            })
            .unwrap();
        if zebra {
            zebras.push(node.0);
        }
    }
    write.commit().unwrap();
    let search = |query: &str| {
        let write = graph.write().unwrap();
        posting_bytes.set(0);
        let mut found: Vec<u64> = write
            .keyword_search("words", ElementKind::Node, query, 10_000)
            .unwrap()
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        found.sort_unstable();
        (found, posting_bytes.get())
    };
    let (_, common) = search("the");
    for query in ["\"zebra the\"", "\"the zebra\""] {
        let (found, phrase) = search(query);
        assert_eq!(found, zebras, "{query}");
        assert!(
            phrase * 4 < common,
            "{query} read {phrase} posting bytes, the common word alone {common}"
        );
    }
}

#[test]
fn a_phrase_misses_a_word_its_transaction_removed_from_a_committed_text() {
    let graph = Graph::open(test_engine::in_memory()).unwrap();
    let mut write = graph.write().unwrap();
    create_indexes(&mut write);
    let label = write.intern_token("Doc").unwrap();
    let text = write.intern_token("text").unwrap();
    let node = write
        .create_node(NodeRecord {
            labels: [label].into(),
            properties: Properties::from([(text, PropertyValue::String("red blue".to_owned()))]),
        })
        .unwrap();
    write.commit().unwrap();
    let mut write = graph.write().unwrap();
    write
        .set_node_property(node, text, PropertyValue::String("red".to_owned()))
        .unwrap();
    for query in ["\"red blue\"", "\"blue red\""] {
        assert_eq!(
            write
                .keyword_search("words", ElementKind::Node, query, 10)
                .unwrap(),
            vec![],
            "{query}"
        );
    }
}

#[test]
fn a_word_its_transaction_removed_from_a_committed_text_is_not_found() {
    for after in [Some("blue blue"), None] {
        let graph = Graph::open(test_engine::in_memory()).unwrap();
        let mut write = graph.write().unwrap();
        create_indexes(&mut write);
        let label = write.intern_token("Doc").unwrap();
        let text = write.intern_token("text").unwrap();
        let node = write
            .create_node(NodeRecord {
                labels: [label].into(),
                properties: Properties::from([(
                    text,
                    PropertyValue::String("red blue".to_owned()),
                )]),
            })
            .unwrap();
        write.commit().unwrap();
        let mut write = graph.write().unwrap();
        match after {
            Some(words) => write
                .set_node_property(node, text, PropertyValue::String(words.to_owned()))
                .unwrap(),
            None => write.remove_node_property(node, text).unwrap(),
        }
        for query in ["red", "\"red blue\""] {
            assert_eq!(
                write
                    .keyword_search("words", ElementKind::Node, query, 10)
                    .unwrap(),
                vec![],
                "{query} after the text became {after:?}"
            );
        }
    }
}

#[test]
fn a_phrase_counts_the_blocks_its_transaction_wrote_when_it_picks_the_rarest_word() {
    let posting_gets = Rc::new(Cell::new(0));
    let graph = Graph::open(CountingStorage {
        inner: test_engine::in_memory(),
        entry_writes: Rc::default(),
        posting_bytes: Rc::default(),
        posting_gets: Rc::clone(&posting_gets),
        log: Rc::default(),
    })
    .unwrap();
    let mut write = graph.write().unwrap();
    create_indexes(&mut write);
    let label = write.intern_token("Doc").unwrap();
    let text = write.intern_token("text").unwrap();
    let create = |write: &mut GraphTx<_>, words: &str| {
        write
            .create_node(NodeRecord {
                labels: [label].into(),
                properties: Properties::from([(text, PropertyValue::String(words.to_owned()))]),
            })
            .unwrap()
            .0
    };
    let mut zebras = Vec::new();
    for words in ["zebra", "zebra", "zebra", "zebra the"] {
        zebras.push(create(&mut write, words));
        for _ in 0..128 {
            write.create_node(NodeRecord::default()).unwrap();
        }
    }
    write.commit().unwrap();
    let mut write = graph.write().unwrap();
    for _ in 0..2000 {
        create(&mut write, "the cat");
    }
    posting_gets.set(0);
    let found = write
        .keyword_search("words", ElementKind::Node, "\"zebra the\"", 10)
        .unwrap();
    assert_eq!(
        found.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        vec![zebras[3]]
    );
    assert!(
        posting_gets.get() <= zebras.len(),
        "{} posting blocks read one by one",
        posting_gets.get()
    );
}

#[test]
fn zero_vector_scores_of_either_sign_tie_in_ascending_id_order() {
    let graph = Graph::open(test_engine::in_memory()).unwrap();
    let mut write = graph.write().unwrap();
    write
        .create_vector_index(
            "dots",
            IndexTarget::Label("Doc"),
            "at",
            2,
            VectorMetric::DotProduct,
        )
        .unwrap();
    let label = write.intern_token("Doc").unwrap();
    let at = write.intern_token("at").unwrap();
    for vector in [[-1.0, -2.0], [1.0, 2.0]] {
        write
            .create_node(NodeRecord {
                labels: [label].into(),
                properties: Properties::from([(
                    at,
                    PropertyValue::List(vector.map(PropertyValue::Float).to_vec()),
                )]),
            })
            .unwrap();
    }
    write.commit().unwrap();
    let found = graph
        .read()
        .unwrap()
        .vector_search("dots", ElementKind::Node, &[0.0, 0.0], 10)
        .unwrap();
    assert_eq!(
        found
            .iter()
            .map(|(id, score)| (*id, score.to_bits()))
            .collect::<Vec<_>>(),
        vec![(0, 0.0f64.to_bits()), (1, 0.0f64.to_bits())]
    );
}

struct Grouped {
    graph: Graph<TestStorage>,
    index: u32,
    group: Token<PropertyKey>,
    label: Token<Label>,
    nodes: Vec<NodeId>,
}

fn memtable_resident_halves(nodes: i64) -> Grouped {
    let graph = Graph::open(test_engine::in_memory()).unwrap();
    let mut write = graph.write().unwrap();
    let label = write.intern_token("P").unwrap();
    let group = write.intern_token("g").unwrap();
    write
        .create_property_index("p g", IndexTarget::Label("P"), "g")
        .unwrap();
    let nodes = (0..nodes)
        .map(|at| {
            write
                .create_node(NodeRecord {
                    labels: [label].into(),
                    properties: Properties::from([(group, PropertyValue::Integer(at % 2))]),
                })
                .unwrap()
        })
        .collect();
    let index = write
        .property_index(Target::Label(label), group)
        .unwrap()
        .unwrap();
    write.commit().unwrap();
    Grouped {
        graph,
        index,
        group,
        label,
        nodes,
    }
}

#[test]
fn a_capped_seek_over_memtable_entries_copies_about_as_many_as_it_reads() {
    let Grouped { graph, index, .. } = memtable_resident_halves(20_000);
    let read = graph.read().unwrap();
    let zero = KeyRange::equal_to(&PropertyValue::Integer(0)).unwrap();
    for limit in [0, 1, 15, 100, 1000] {
        let before = graph.storage().memtable_copied();
        assert_eq!(
            read.property_seek_within(index, std::slice::from_ref(&zero), limit)
                .unwrap(),
            None
        );
        let copied = graph.storage().memtable_copied() - before;
        assert!(
            copied <= 2 * (limit as u64 + 1) + 16,
            "a seek capped at {limit} copied {copied} entries"
        );
    }
    let before = graph.storage().memtable_copied();
    assert_eq!(
        read.property_seek_within(index, &[zero], 10_000)
            .unwrap()
            .map(|found| found.len()),
        Some(10_000)
    );
    assert!(graph.storage().memtable_copied() - before <= 2 * 10_001);
}

#[test]
fn a_capped_seek_counts_the_writes_its_transaction_has_not_committed() {
    let Grouped {
        graph,
        index,
        group,
        label,
        nodes,
    } = memtable_resident_halves(200);
    let mut write = graph.write().unwrap();
    for &node in nodes[..100].iter().step_by(2) {
        write.delete_node(node).unwrap();
    }
    for &node in nodes[1..40].iter().step_by(2) {
        write
            .set_node_property(node, group, PropertyValue::Integer(0))
            .unwrap();
    }
    for _ in 0..7 {
        write
            .create_node(NodeRecord {
                labels: [label].into(),
                properties: Properties::from([(group, PropertyValue::Integer(0))]),
            })
            .unwrap();
    }
    let zero = KeyRange::equal_to(&PropertyValue::Integer(0)).unwrap();
    let all = write
        .property_seek(index, std::slice::from_ref(&zero))
        .unwrap();
    assert_eq!(all.len(), 50 + 20 + 7);
    let within = |limit| {
        write
            .property_seek_within(index, std::slice::from_ref(&zero), limit)
            .unwrap()
    };
    assert_eq!(within(all.len()), Some(all.clone()));
    assert_eq!(within(all.len() - 1), None);
    assert_eq!(within(usize::MAX), Some(all));
}

fn create_sentence(
    write: &mut GraphTx<impl WriteTransaction>,
    names: &Names,
    gap: usize,
    picks: &[usize],
) -> u64 {
    for _ in 0..gap {
        write.create_node(NodeRecord::default()).unwrap();
    }
    let words = ["red", "green", "blue"];
    let text = picks.iter().map(|pick| words[*pick]).collect::<Vec<_>>();
    write
        .create_node(NodeRecord {
            labels: [names.label].into(),
            properties: Properties::from([(names.text, PropertyValue::String(text.join(" ")))]),
        })
        .unwrap()
        .0
}

#[derive(Clone, Debug)]
enum Change {
    SetText(Vec<usize>),
    RemoveText,
    RemoveLabel,
    Delete,
}

fn change() -> impl Strategy<Value = Change> {
    prop_oneof![
        3 => prop::collection::vec(0usize..3, 0..6).prop_map(Change::SetText),
        1 => Just(Change::RemoveText),
        1 => Just(Change::RemoveLabel),
        1 => Just(Change::Delete),
    ]
}

fn phrase_hits(read: &GraphTx<impl ReadTransaction>, phrase: &[usize]) -> BTreeSet<u64> {
    let words = ["red", "green", "blue"];
    let text = phrase.iter().map(|pick| words[*pick]).collect::<Vec<_>>();
    read.keyword_search(
        "words",
        ElementKind::Node,
        &format!("\"{}\"", text.join(" ")),
        usize::MAX,
    )
    .unwrap()
    .into_iter()
    .map(|(id, _)| id)
    .collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    #[ignore = "left out of mise run check, run by mise run random"]
    fn a_phrase_finds_exactly_the_nodes_that_hold_it_before_and_after_commit(
        committed in prop::collection::vec((0usize..160, prop::collection::vec(0usize..3, 0..6)), 0..8),
        written in prop::collection::vec((0usize..160, prop::collection::vec(0usize..3, 0..6)), 0..8),
        changes in prop::collection::vec((any::<prop::sample::Index>(), change()), 0..6),
        phrase in prop::collection::vec(0usize..3, 1..4),
    ) {
        let graph = Graph::open(test_engine::in_memory()).unwrap();
        let mut write = graph.write().unwrap();
        create_indexes(&mut write);
        let names = Names {
            label: write.intern_token("Doc").unwrap(),
            link: write.intern_token("LINK").unwrap(),
            other: write.intern_token("OTHER").unwrap(),
            text: write.intern_token("text").unwrap(),
            tags: write.intern_token("tags").unwrap(),
            at: write.intern_token("at").unwrap(),
        };
        let mut sentences = BTreeMap::new();
        for (gap, picks) in &committed {
            sentences.insert(create_sentence(&mut write, &names, *gap, picks), picks.clone());
        }
        write.commit().unwrap();
        let mut write = graph.write().unwrap();
        for (gap, picks) in &written {
            sentences.insert(create_sentence(&mut write, &names, *gap, picks), picks.clone());
        }
        let ids: Vec<u64> = sentences.keys().copied().collect();
        for (index, change) in &changes {
            if ids.is_empty() {
                break;
            }
            let id = ids[index.index(ids.len())];
            if !sentences.contains_key(&id) {
                continue;
            }
            let node = NodeId(id);
            match change {
                Change::SetText(picks) => {
                    let words = ["red", "green", "blue"];
                    let text = picks.iter().map(|pick| words[*pick]).collect::<Vec<_>>();
                    write
                        .set_node_property(node, names.text, PropertyValue::String(text.join(" ")))
                        .unwrap();
                    sentences.insert(id, picks.clone());
                }
                Change::RemoveText => {
                    write.remove_node_property(node, names.text).unwrap();
                    sentences.remove(&id);
                }
                Change::RemoveLabel => {
                    write.remove_labels(node, &[names.label]).unwrap();
                    sentences.remove(&id);
                }
                Change::Delete => {
                    write.delete_node(node).unwrap();
                    sentences.remove(&id);
                }
            }
        }
        let expected: BTreeSet<u64> = sentences
            .iter()
            .filter(|(_, picks)| picks.windows(phrase.len()).any(|window| window == phrase))
            .map(|(id, _)| *id)
            .collect();
        prop_assert_eq!(phrase_hits(&write, &phrase), expected.clone());
        write.commit().unwrap();
        prop_assert_eq!(phrase_hits(&graph.read().unwrap(), &phrase), expected);
    }
}
