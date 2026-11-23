use std::collections::BTreeMap;
use std::rc::Rc;

use proptest::collection::vec;
use proptest::prelude::*;
use proptest::test_runner::TestCaseError;

use super::Entry;
use super::ReadTransaction;
use super::Storage;
use super::StorageError;
use super::Table;
use super::WriteTransaction;
use super::conformance::Harness;

const TABLES: [Table; 3] = [Table::Metadata, Table::NodeColumns, Table::PropertyEntries];

/// One step a model run takes on the store under test and on the model alike.
#[derive(Clone, Debug)]
pub(crate) enum Op {
    BeginWrite,
    Put {
        table: usize,
        key: Vec<u8>,
        length: usize,
    },
    Delete {
        table: usize,
        key: Vec<u8>,
    },
    DeleteRange {
        table: usize,
        start: Vec<u8>,
        end: Vec<u8>,
    },
    Commit,
    Rollback,
    DropWrite,
    BeginRead,
    DropRead {
        reader: usize,
    },
    Read {
        at: At,
        query: Query,
    },
    Reopen,
}

/// The transaction a [`Op::Read`] reads through.
#[derive(Clone, Debug)]
pub(crate) enum At {
    Writer,
    Reader(usize),
    Latest,
}

#[derive(Clone, Debug)]
pub(crate) enum Query {
    Get {
        table: usize,
        key: Vec<u8>,
    },
    Prefix {
        table: usize,
        prefix: Vec<u8>,
    },
    Range {
        table: usize,
        start: Vec<u8>,
        end: Vec<u8>,
    },
    GetWith {
        table: usize,
        keys: Vec<Vec<u8>>,
    },
}

type Snapshot = BTreeMap<(usize, Vec<u8>), Vec<u8>>;

fn key() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        6 => vec(prop_oneof![Just(0u8), Just(1), Just(0xFF)], 0..3),
        4 => (0u64..10).prop_map(|id| id.to_be_bytes().to_vec()),
        1 => any::<u64>().prop_map(|id| id.to_be_bytes().to_vec()),
    ]
}

fn length() -> impl Strategy<Value = usize> {
    prop_oneof![
        12 => 0usize..24,
        1 => Just(1024),
        1 => Just(4096),
        1 => Just(4097),
        1 => Just(70_000),
    ]
}

fn table() -> impl Strategy<Value = usize> {
    0..TABLES.len()
}

fn query() -> impl Strategy<Value = Query> {
    prop_oneof![
        3 => (table(), key()).prop_map(|(table, key)| Query::Get { table, key }),
        2 => (table(), vec(prop_oneof![Just(0u8), Just(1), Just(0xFF)], 0..3))
            .prop_map(|(table, prefix)| Query::Prefix { table, prefix }),
        2 => (table(), key(), key()).prop_map(|(table, start, end)| Query::Range {
            table,
            start,
            end,
        }),
        2 => (table(), vec(key(), 0..12))
            .prop_map(|(table, keys)| Query::GetWith { table, keys }),
    ]
}

fn at() -> impl Strategy<Value = At> {
    prop_oneof![
        Just(At::Writer),
        (0usize..4).prop_map(At::Reader),
        Just(At::Latest),
    ]
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => Just(Op::BeginWrite),
        6 => (table(), key(), length()).prop_map(|(table, key, length)| Op::Put {
            table,
            key,
            length,
        }),
        2 => (table(), key()).prop_map(|(table, key)| Op::Delete { table, key }),
        1 => (table(), key(), key()).prop_map(|(table, start, end)| Op::DeleteRange {
            table,
            start,
            end,
        }),
        2 => Just(Op::Commit),
        1 => Just(Op::Rollback),
        1 => Just(Op::DropWrite),
        2 => Just(Op::BeginRead),
        1 => (0usize..4).prop_map(|reader| Op::DropRead { reader }),
        6 => (at(), query()).prop_map(|(at, query)| Op::Read { at, query }),
        1 => Just(Op::Reopen),
    ]
}

/// Sequences of model steps, biased toward shared key prefixes, fixed-width IDs, values around
/// page sizes, and readers pinned across commits.
pub(crate) fn ops() -> impl Strategy<Value = Vec<Op>> {
    vec(op(), 1..100)
}

/// Runs `ops` on the harness's store and on a model of committed versions, and fails at the
/// first read where they differ. Every reader pins the version that was latest when it began.
pub(crate) fn check<H: Harness>(harness: &mut H, ops: &[Op]) -> Result<(), TestCaseError> {
    let mut model = Model::default();
    let mut rest = ops;
    loop {
        let store = harness.open();
        model.compare_everything(&store)?;
        match model.epoch(&store, rest)? {
            Some(after_reopen) => rest = after_reopen,
            None => {
                model.compare_everything(&store)?;
                return Ok(());
            }
        }
    }
}

#[derive(Default)]
struct Model {
    committed: Rc<Snapshot>,
    stamp: u32,
}

struct Writer<W> {
    transaction: W,
    pending: Snapshot,
}

impl Model {
    fn value(&mut self, length: usize) -> Vec<u8> {
        self.stamp += 1;
        let stamp = self.stamp.to_le_bytes();
        (0..length)
            .map(|index| stamp[index % 4] ^ (index / 4) as u8)
            .collect()
    }

    fn epoch<'s, 'o, S: Storage>(
        &mut self,
        store: &'s S,
        ops: &'o [Op],
    ) -> Result<Option<&'o [Op]>, TestCaseError> {
        let mut writer: Option<Writer<S::Write<'s>>> = None;
        let mut readers: Vec<(S::Read<'s>, Rc<Snapshot>)> = Vec::new();
        for (index, op) in ops.iter().enumerate() {
            match op {
                Op::Reopen => return Ok(Some(&ops[index + 1..])),
                Op::BeginWrite => {
                    if writer.is_none() {
                        writer = Some(Writer {
                            transaction: store.begin_write().unwrap(),
                            pending: (*self.committed).clone(),
                        });
                    }
                }
                Op::Put { table, key, length } => {
                    if let Some(writer) = &mut writer {
                        let value = self.value(*length);
                        writer.transaction.put(TABLES[*table], key, &value).unwrap();
                        writer.pending.insert((*table, key.clone()), value);
                    }
                }
                Op::Delete { table, key } => {
                    if let Some(writer) = &mut writer {
                        writer.transaction.delete(TABLES[*table], key).unwrap();
                        writer.pending.remove(&(*table, key.clone()));
                    }
                }
                Op::DeleteRange { table, start, end } => {
                    if let Some(writer) = &mut writer {
                        writer
                            .transaction
                            .delete_range(TABLES[*table], start, end)
                            .unwrap();
                        writer
                            .pending
                            .retain(|(held, key), _| *held != *table || key < start || key >= end);
                    }
                }
                Op::Commit => {
                    if let Some(writer) = writer.take() {
                        writer.transaction.commit().unwrap();
                        self.committed = Rc::new(writer.pending);
                    }
                }
                Op::Rollback => {
                    if let Some(writer) = writer.take() {
                        writer.transaction.rollback().unwrap();
                    }
                }
                Op::DropWrite => drop(writer.take()),
                Op::BeginRead => {
                    readers.push((store.begin_read().unwrap(), Rc::clone(&self.committed)));
                }
                Op::DropRead { reader } => {
                    if !readers.is_empty() {
                        let reader = reader % readers.len();
                        readers.remove(reader);
                    }
                }
                Op::Read { at, query } => match at {
                    At::Writer => {
                        if let Some(writer) = &writer {
                            check_query(&writer.transaction, &writer.pending, query)?;
                        }
                    }
                    At::Reader(reader) => {
                        if !readers.is_empty() {
                            let (read, snapshot) = &readers[reader % readers.len()];
                            check_query(read, snapshot, query)?;
                        }
                    }
                    At::Latest => {
                        check_query(&store.begin_read().unwrap(), &self.committed, query)?;
                    }
                },
            }
        }
        for (read, snapshot) in &readers {
            compare_tables(read, snapshot)?;
        }
        Ok(None)
    }

    fn compare_everything(&self, store: &impl Storage) -> Result<(), TestCaseError> {
        compare_tables(&store.begin_read().unwrap(), &self.committed)
    }
}

fn compare_tables(read: &impl ReadTransaction, snapshot: &Snapshot) -> Result<(), TestCaseError> {
    for (index, table) in TABLES.into_iter().enumerate() {
        prop_assert_eq!(
            read.scan_prefix(table, &[]).unwrap(),
            expected_prefix(snapshot, index, &[]),
            "table {:?}",
            table
        );
    }
    Ok(())
}

fn expected_prefix(snapshot: &Snapshot, table: usize, prefix: &[u8]) -> Vec<Entry> {
    snapshot
        .range((table, prefix.to_vec())..)
        .take_while(|((found, key), _)| *found == table && key.starts_with(prefix))
        .map(|((_, key), value)| (key.clone(), value.clone()))
        .collect()
}

fn expected_range(snapshot: &Snapshot, table: usize, start: &[u8], end: &[u8]) -> Vec<Entry> {
    if start >= end {
        return Vec::new();
    }
    snapshot
        .range((table, start.to_vec())..(table, end.to_vec()))
        .map(|((_, key), value)| (key.clone(), value.clone()))
        .collect()
}

fn check_query(
    read: &impl ReadTransaction,
    snapshot: &Snapshot,
    query: &Query,
) -> Result<(), TestCaseError> {
    match query {
        Query::Get { table, key } => {
            let expected = snapshot.get(&(*table, key.clone())).cloned();
            prop_assert_eq!(&read.get(TABLES[*table], key).unwrap(), &expected);
            let copied = read
                .get_with(TABLES[*table], key, |value| {
                    Ok::<_, StorageError>(value.to_vec())
                })
                .unwrap();
            prop_assert_eq!(copied, expected);
        }
        Query::Prefix { table, prefix } => {
            let expected = expected_prefix(snapshot, *table, prefix);
            prop_assert_eq!(
                &read.scan_prefix(TABLES[*table], prefix).unwrap(),
                &expected
            );
            let mut visited = Vec::new();
            read.visit_prefix(TABLES[*table], prefix, |key, value| {
                visited.push((key.to_vec(), value.to_vec()));
                Ok::<_, StorageError>(())
            })
            .unwrap();
            prop_assert_eq!(visited, expected);
        }
        Query::Range { table, start, end } => {
            prop_assert_eq!(
                read.scan_range(TABLES[*table], start, end).unwrap(),
                expected_range(snapshot, *table, start, end)
            );
        }
        Query::GetWith { table, keys } => {
            for key in keys {
                let found = read
                    .get_with(TABLES[*table], key, |value| {
                        Ok::<_, StorageError>(value.to_vec())
                    })
                    .unwrap();
                prop_assert_eq!(
                    found,
                    snapshot.get(&(*table, key.clone())).cloned(),
                    "borrowed read of {:?}",
                    key
                );
            }
        }
    }
    Ok(())
}
