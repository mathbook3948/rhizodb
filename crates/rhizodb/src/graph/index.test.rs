use std::cell::Cell;
use std::rc::Rc;

use proptest::prelude::*;

use super::*;
use crate::graph::Direction;
use crate::graph::Graph;
use crate::graph::NodeId;
use crate::graph::RelationshipId;
use crate::graph::records::Properties;
use crate::storage::Storage;
use crate::storage::StorageError;
use crate::storage::redb_backend::RedbRead;
use crate::storage::redb_backend::RedbStorage;
use crate::storage::redb_backend::RedbWrite;

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
            write.create_node(&record).unwrap();
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
            for name in ["words", "points", "link words", "link points"] {
                write.drop_index(name).unwrap();
            }
            create_indexes(write);
            create_relationship_indexes(write);
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

fn store_label_only_definitions(write: &mut impl WriteTransaction) {
    for (name, value) in write.scan_prefix(Table::Indexes, &[]).unwrap() {
        assert_eq!(value[4], TARGET_LABEL);
        let label_only = [&value[..4], &value[5..]].concat();
        write.put(Table::Indexes, &name, &label_only).unwrap();
    }
}

#[test]
fn opening_a_database_of_format_four_tags_each_index_as_one_over_a_label() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("four.redb");
    {
        let graph = Graph::open(RedbStorage::open(&path).unwrap()).unwrap();
        let mut write = graph.write().unwrap();
        create_indexes(&mut write);
        let label = write.intern_token("Doc").unwrap();
        let text = write.intern_token("text").unwrap();
        let at = write.intern_token("at").unwrap();
        write
            .create_node(&NodeRecord {
                labels: [label].into(),
                properties: Properties::from([
                    (text, PropertyValue::String("graph store".to_owned())),
                    (
                        at,
                        PropertyValue::List(vec![
                            PropertyValue::Integer(1),
                            PropertyValue::Integer(0),
                        ]),
                    ),
                ]),
            })
            .unwrap();
        write.commit().unwrap();

        let mut previous = graph.storage.begin_write().unwrap();
        store_label_only_definitions(&mut previous);
        previous
            .put(
                Table::Metadata,
                keys::FORMAT_VERSION_KEY,
                &4u32.to_be_bytes(),
            )
            .unwrap();
        previous.commit().unwrap();
        assert!(graph.read().unwrap().indexes().is_err());
    }
    let graph = Graph::open(RedbStorage::open(&path).unwrap()).unwrap();
    let read = graph.read().unwrap();
    assert_eq!(read.check_integrity().unwrap(), vec![]);
    let targets: Vec<Target> = read
        .indexes()
        .unwrap()
        .into_iter()
        .map(|(_, index)| index.definition.target)
        .collect();
    let label = read.lookup_token::<Label>("Doc").unwrap().unwrap();
    assert_eq!(targets, [Target::Label(label), Target::Label(label)]);
    assert_eq!(
        read.keyword_search("words", ElementKind::Node, "graph", 10)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        read.vector_search("points", ElementKind::Node, &[1.0, 1.0], 10)
            .unwrap()
            .len(),
        1
    );
}

#[derive(Debug)]
struct CountingStorage {
    inner: RedbStorage,
    entry_writes: Rc<Cell<usize>>,
}

struct CountingWrite {
    inner: RedbWrite,
    entry_writes: Rc<Cell<usize>>,
}

impl CountingWrite {
    fn count(&self, table: Table) {
        if ENTRY_TABLES.contains(&table) {
            self.entry_writes.set(self.entry_writes.get() + 1);
        }
    }
}

impl Storage for CountingStorage {
    type Read<'a> = RedbRead;
    type Write<'a> = CountingWrite;

    fn begin_read(&self) -> Result<RedbRead, StorageError> {
        self.inner.begin_read()
    }

    fn begin_write(&self) -> Result<CountingWrite, StorageError> {
        Ok(CountingWrite {
            inner: self.inner.begin_write()?,
            entry_writes: Rc::clone(&self.entry_writes),
        })
    }
}

impl ReadTransaction for CountingWrite {
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
}

impl WriteTransaction for CountingWrite {
    fn put(&mut self, table: Table, key: &[u8], value: &[u8]) -> Result<(), StorageError> {
        self.count(table);
        self.inner.put(table, key, value)
    }

    fn delete(&mut self, table: Table, key: &[u8]) -> Result<(), StorageError> {
        self.count(table);
        self.inner.delete(table, key)
    }

    fn commit(self) -> Result<(), StorageError> {
        self.inner.commit()
    }

    fn rollback(self) -> Result<(), StorageError> {
        self.inner.rollback()
    }
}

#[test]
fn opening_a_database_of_format_four_keeps_its_keyword_entries() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("four.redb");
    {
        let graph = Graph::open(RedbStorage::open(&path).unwrap()).unwrap();
        let mut write = graph.write().unwrap();
        create_indexes(&mut write);
        let label = write.intern_token("Doc").unwrap();
        let text = write.intern_token("text").unwrap();
        let at = write.intern_token("at").unwrap();
        write
            .create_node(&NodeRecord {
                labels: [label].into(),
                properties: Properties::from([
                    (text, PropertyValue::String("graph store graph".to_owned())),
                    (
                        at,
                        PropertyValue::List(vec![
                            PropertyValue::Float(1.0),
                            PropertyValue::Float(0.0),
                        ]),
                    ),
                ]),
            })
            .unwrap();
        write.commit().unwrap();
        let mut previous = graph.storage.begin_write().unwrap();
        store_label_only_definitions(&mut previous);
        previous
            .put(
                Table::Metadata,
                keys::FORMAT_VERSION_KEY,
                &4u32.to_be_bytes(),
            )
            .unwrap();
        previous.commit().unwrap();
    }
    let entry_writes = Rc::new(Cell::new(0));
    let graph = Graph::open(CountingStorage {
        inner: RedbStorage::open(&path).unwrap(),
        entry_writes: Rc::clone(&entry_writes),
    })
    .unwrap();
    assert_eq!(entry_writes.get(), 0);
    let read = graph.read().unwrap();
    assert_eq!(read.check_integrity().unwrap(), vec![]);
    assert_eq!(
        read.keyword_search("words", ElementKind::Node, "\"graph store\"", 10)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        read.vector_search("points", ElementKind::Node, &[1.0, 0.0], 10)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn opening_a_database_of_format_three_rebuilds_its_keyword_indexes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("previous.redb");
    {
        let graph = Graph::open(RedbStorage::open(&path).unwrap()).unwrap();
        let mut write = graph.write().unwrap();
        write
            .create_keyword_index("words", IndexTarget::Label("Doc"), &["text"])
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
        store_label_only_definitions(&mut previous);
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
        read.keyword_search("words", ElementKind::Node, "\"graph store\"", 10)
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

#[test]
fn counts_read_the_frequency_and_length_of_a_posting_without_its_positions() {
    let posting = encode_posting(&[0, 5, 9], 12);
    assert_eq!(decode_counts(&posting).unwrap(), (3, 12));
    assert_eq!(decode_posting(&posting).unwrap().positions, [0, 5, 9]);
}

#[test]
fn counts_of_a_posting_cut_short_of_its_positions_fail_to_decode() {
    let posting = encode_posting(&[0, 5, 9], 12);
    assert!(decode_counts(&posting[..10]).is_err());
    assert!(decode_counts(&posting[..8]).is_err());
    assert!(decode_counts(&posting[..7]).is_err());
}

#[test]
fn creating_a_relationship_index_fills_it_from_relationships_of_its_type_alone() {
    let graph = Graph::open(RedbStorage::in_memory().unwrap()).unwrap();
    let mut write = graph.write().unwrap();
    let link = write.intern_token("LINK").unwrap();
    let other = write.intern_token("OTHER").unwrap();
    let text = write.intern_token("text").unwrap();
    let mut node = || {
        write
            .create_node(&NodeRecord {
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
    let graph = Graph::open(RedbStorage::in_memory().unwrap()).unwrap();
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
    let first = write.create_node(&doc).unwrap();
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
    let second = write.create_node(&doc).unwrap();
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
    write.create_node(&doc).unwrap();
    write.remove_node_property(first, text).unwrap();
    assert_eq!(words(&write, ElementKind::Node), 2);
    write.drop_index("words").unwrap();
    write.create_node(&doc).unwrap();

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
