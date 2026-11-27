use super::tests::store_as_format_fourteen;
use super::*;
use crate::engine::sim_fs::Crash;
use crate::engine::sim_fs::Failure;
use crate::engine::sim_fs::SimFs;
use crate::storage::test_engine;
use crate::storage::test_engine::TestStorage;
use crate::test_scale;

type Expected = BTreeMap<RelationshipId, RelationshipRecord>;

fn prepare(fs: &SimFs) -> Expected {
    let graph = Graph::open(test_engine::on_sim(fs).unwrap()).unwrap();
    let mut expected = Expected::new();
    let mut write = graph.write().unwrap();
    let rel_type = write.intern_token::<RelationshipType>("R").unwrap();
    let key = write.intern_token::<PropertyKey>("k").unwrap();
    let text = write.intern_token::<PropertyKey>("s").unwrap();
    let nodes: Vec<NodeId> = (0..20)
        .map(|_| {
            write
                .create_node(NodeRecord {
                    labels: BTreeSet::new(),
                    properties: Properties::new(),
                })
                .unwrap()
        })
        .collect();
    for index in 0..4000usize {
        let mut record = RelationshipRecord {
            rel_type,
            source: nodes[index % nodes.len()],
            target: nodes[index * 7 % nodes.len()],
            properties: Properties::new(),
        };
        if index % 3 != 0 {
            record
                .properties
                .insert(key, PropertyValue::Integer(index as i64));
        }
        record.properties.insert(
            text,
            PropertyValue::String(format!("relationship {index:08}")),
        );
        let id = write.create_relationship(&record).unwrap();
        if index % 11 == 4 || (100..140).contains(&index) {
            write.delete_relationship(id).unwrap();
        } else {
            expected.insert(id, record);
        }
    }
    write.commit().unwrap();
    let mut previous = graph.storage().begin_write().unwrap();
    store_as_format_fourteen(&mut previous);
    previous.commit().unwrap();
    expected
}

fn stored_format(fs: &SimFs) -> u32 {
    let stored = test_engine::on_sim(fs)
        .unwrap()
        .begin_read()
        .unwrap()
        .get(Table::Metadata, keys::FORMAT_VERSION_KEY)
        .unwrap()
        .unwrap();
    decode_u32(&stored).unwrap()
}

fn assert_holds(graph: &Graph<TestStorage>, expected: &Expected, context: &str) {
    let read = graph.read().unwrap();
    for (id, record) in expected {
        assert_eq!(
            read.relationship(*id).unwrap().as_ref(),
            Some(record),
            "{context}"
        );
    }
    let mut count = 0;
    read.visit_relationship_records(|_, _| {
        count += 1;
        Ok(())
    })
    .unwrap();
    assert_eq!(count, expected.len(), "{context}");
    assert_eq!(read.check_integrity().unwrap(), vec![], "{context}");
    graph.check_relationship_chunks().unwrap();
}

#[test]
#[ignore = "left out of mise run check, run by mise run random"]
fn a_crash_at_any_write_of_the_upgrade_from_format_fourteen_resumes_to_every_relationship() {
    let prepared = SimFs::new();
    let expected = prepare(&prepared);
    let operations = {
        let fs = prepared.crash(Crash::KeepUnsynced);
        let storage = test_engine::on_sim(&fs).unwrap();
        let start = fs.operations();
        Graph::open(storage).unwrap();
        fs.operations() - start
    };
    assert!(operations > 0);
    let mut modes = vec![Crash::DropUnsynced, Crash::KeepUnsynced];
    modes.extend((0..u64::from(test_scale::cases(2))).map(|seed| Crash::Scramble { seed }));
    let mut outcomes = BTreeSet::new();
    for failure in [Failure::FromThenOn, Failure::Once] {
        for at in 0..=operations {
            let fs = prepared.crash(Crash::KeepUnsynced);
            let upgraded = {
                let storage = test_engine::on_sim(&fs).unwrap();
                fs.fail_after(at, failure);
                Graph::open(storage).is_ok()
            };
            for &mode in &modes {
                let context = format!("{failure:?} at {at} of {operations}, {mode:?}");
                let recovered = fs.crash(mode);
                let format = stored_format(&recovered);
                assert!(
                    [14, 14 | UPGRADING, FORMAT_VERSION].contains(&format),
                    "{context}"
                );
                if upgraded {
                    assert_eq!(
                        format, FORMAT_VERSION,
                        "{context}: an upgrade that returned is lost"
                    );
                }
                outcomes.insert(format);
                let graph = match test_engine::on_sim(&recovered)
                    .map_err(GraphError::from)
                    .and_then(Graph::open)
                {
                    Ok(graph) => graph,
                    Err(error) => panic!("{context}: {error}"),
                };
                assert_holds(&graph, &expected, &context);
            }
        }
    }
    assert_eq!(
        outcomes,
        [14, 14 | UPGRADING, FORMAT_VERSION].into(),
        "the crashes land before, inside, and after the upgrade"
    );
}
