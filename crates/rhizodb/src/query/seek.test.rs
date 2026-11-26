use std::collections::BTreeMap;
use std::sync::Arc;

use proptest::prelude::*;

use super::*;
use crate::Database;
use crate::IndexTarget as PublicTarget;
use crate::Parameters;
use crate::Value;
use crate::engine::any_fs::open_path;
use crate::engine::store::EngineOptions;
use crate::graph::Graph;
use crate::graph::IndexTarget;
use crate::graph::NodeRecord;
use crate::graph::Properties;

use crate::query::compare::Comparison;
use crate::query::compare::compare;
use crate::query::compare::equals;
use crate::storage::Storage;
use crate::storage::Table;
use crate::storage::WriteTransaction;
use crate::storage::test_engine;
use crate::temporal::Date;
use crate::temporal::DateTime;
use crate::temporal::Duration;
use crate::temporal::LocalDateTime;
use crate::temporal::LocalTime;
use crate::temporal::Temporal;
use crate::temporal::Time;
use crate::temporal::Zone;

const TWO_TO_53: i64 = 1 << 53;

const INTEGERS: [i64; 9] = [
    i64::MIN,
    -TWO_TO_53 - 1,
    -1,
    0,
    1,
    2,
    TWO_TO_53,
    TWO_TO_53 + 1,
    i64::MAX,
];

const FLOATS: [f64; 16] = [
    f64::NEG_INFINITY,
    -18_446_744_073_709_551_616.0,
    -9_223_372_036_854_775_808.0,
    -9_007_199_254_740_992.0,
    -1.5,
    -0.5,
    -0.0,
    0.0,
    0.5,
    1.0,
    2.0,
    9_007_199_254_740_992.0,
    9_223_372_036_854_775_808.0,
    18_446_744_073_709_551_616.0,
    f64::INFINITY,
    f64::NAN,
];

const BOUNDS: [Bound; 4] = [
    Bound::Less,
    Bound::LessEqual,
    Bound::Greater,
    Bound::GreaterEqual,
];

fn temporal() -> impl Strategy<Value = Temporal> + Clone {
    let day = 1u32..3;
    let hour = 0u32..3;
    let offset = prop::sample::select(vec![-3600, 0, 3600]);
    let local = |day: u32, hour: u32| {
        LocalDateTime::new(
            Date::from_ymd(2024, 3, day).unwrap(),
            LocalTime::from_hms_nano(hour, 0, 0, 0).unwrap(),
        )
    };
    prop_oneof![
        day.clone()
            .prop_map(|day| Temporal::Date(Date::from_ymd(2024, 3, day).unwrap())),
        hour.clone()
            .prop_map(|hour| Temporal::LocalTime(LocalTime::from_hms_nano(hour, 0, 0, 0).unwrap())),
        (hour.clone(), offset.clone()).prop_map(|(hour, offset)| Temporal::Time(
            Time::new(LocalTime::from_hms_nano(hour, 0, 0, 0).unwrap(), offset).unwrap()
        )),
        (day.clone(), hour.clone())
            .prop_map(move |(day, hour)| Temporal::LocalDateTime(local(day, hour))),
        (day.clone(), hour.clone(), offset).prop_map(move |(day, hour, offset)| {
            Temporal::DateTime(DateTime::with_offset(local(day, hour), offset).unwrap())
        }),
        (day, hour).prop_map(move |(day, hour)| {
            Temporal::DateTime(DateTime::in_zone(local(day, hour), "Europe/Paris").unwrap())
        }),
        (0i64..2, 0i64..2).prop_map(|(months, seconds)| Temporal::Duration(
            Duration::new(months, 0, seconds, 0).unwrap()
        )),
    ]
}

fn scalar() -> impl Strategy<Value = PropertyValue> + Clone {
    prop_oneof![
        any::<bool>().prop_map(PropertyValue::Boolean),
        prop::sample::select(INTEGERS.to_vec()).prop_map(PropertyValue::Integer),
        (-3i64..3).prop_map(PropertyValue::Integer),
        prop::sample::select(FLOATS.to_vec()).prop_map(PropertyValue::Float),
        any::<f64>().prop_map(PropertyValue::Float),
        "[ab\\x00é]{0,3}".prop_map(PropertyValue::String),
        prop::collection::vec(prop::sample::select(vec![0u8, 1, 255]), 0..3)
            .prop_map(PropertyValue::Bytes),
        temporal().prop_map(PropertyValue::Temporal),
    ]
}

fn stored_value() -> impl Strategy<Value = PropertyValue> + Clone {
    let list = prop::collection::vec(scalar(), 0..3).prop_map(PropertyValue::List);
    prop_oneof![
        6 => scalar(),
        2 => list.clone(),
        1 => prop::collection::vec(list, 0..3).prop_map(PropertyValue::List),
    ]
}

fn query_value() -> impl Strategy<Value = Datum> {
    let plain = stored_value().prop_map(Datum::from);
    prop_oneof![
        8 => plain.clone(),
        1 => Just(Datum::Null),
        1 => plain.clone().prop_map(|value| Datum::Map(Arc::new(BTreeMap::from([(Arc::from("k"), value)])))),
        1 => plain.prop_map(|value| Datum::List(Arc::new(vec![value, Datum::Null]))),
    ]
}

fn twin(value: &PropertyValue) -> PropertyValue {
    match value {
        PropertyValue::Integer(number) => PropertyValue::Float(*number as f64),
        PropertyValue::Float(number) if number.fract() == 0.0 && number.abs() < 1e18 => {
            PropertyValue::Integer(*number as i64)
        }
        PropertyValue::Temporal(Temporal::Time(time)) => {
            let nanos = time.local_time().nanos();
            [-3600, 0, 3600]
                .into_iter()
                .filter(|offset| *offset != time.offset_seconds())
                .find_map(|offset| {
                    let shift = i64::from(offset - time.offset_seconds()) * 1_000_000_000;
                    let local = LocalTime::from_nanos(nanos + shift)?;
                    Some(PropertyValue::Temporal(Temporal::Time(
                        Time::new(local, offset).unwrap(),
                    )))
                })
                .unwrap_or_else(|| value.clone())
        }
        PropertyValue::Temporal(Temporal::DateTime(datetime)) => {
            let offset = if datetime.offset_seconds() > 0 {
                -3600
            } else {
                3600
            };
            let moved = DateTime::from_instant(
                datetime.epoch_second(),
                i64::from(datetime.nanosecond()),
                &Zone::Offset(offset),
            );
            PropertyValue::Temporal(Temporal::DateTime(moved.unwrap()))
        }
        PropertyValue::List(items) => PropertyValue::List(items.iter().map(twin).collect()),
        other => other.clone(),
    }
}

fn values() -> impl Strategy<Value = (Vec<PropertyValue>, Datum)> {
    prop::collection::vec(stored_value(), 1..10).prop_flat_map(|stored| {
        let count = stored.len();
        let query = prop_oneof![
            query_value(),
            (0..count, any::<bool>()).prop_map({
                let stored = stored.clone();
                move |(at, twinned)| {
                    let value = stored[at].clone();
                    Datum::from(if twinned { twin(&value) } else { value })
                }
            }),
        ];
        (Just(stored), query)
    })
}

fn holds(bound: Bound, stored: &Datum, value: &Datum) -> bool {
    match compare(stored, value) {
        Some(Comparison::Ordered(ordering)) => match bound {
            Bound::Less => ordering.is_lt(),
            Bound::LessEqual => ordering.is_le(),
            Bound::Greater => ordering.is_gt(),
            Bound::GreaterEqual => ordering.is_ge(),
        },
        _ => false,
    }
}

proptest! {
    #[test]
    fn a_seek_finds_exactly_the_values_that_equal_or_compare_to_its_value(
        (stored, value) in values(),
        fill_on_create in any::<bool>(),
    ) {
        let graph = Graph::open(test_engine::in_memory()).unwrap();
        let mut write = graph.write().unwrap();
        let label: Token<Label> = write.intern_token("P").unwrap();
        let key: Token<PropertyKey> = write.intern_token("k").unwrap();
        if !fill_on_create {
            write.create_property_index("i", IndexTarget::Label("P"), "k").unwrap();
        }
        let mut nodes = Vec::new();
        for item in &stored {
            let mut properties = Properties::new();
            properties.insert(key, item.clone());
            let record = NodeRecord {
                labels: [label].into_iter().collect(),
                properties,
            };
            nodes.push((write.create_node(record.clone()).unwrap().0, Datum::from(item.clone())));
        }
        if fill_on_create {
            write.create_property_index("i", IndexTarget::Label("P"), "k").unwrap();
        }
        let index = write.property_index(Target::Label(label), key).unwrap().unwrap();
        let seek = |range: KeyRange| write.property_seek(index, &[range]).unwrap();
        let matching = |test: &dyn Fn(&Datum) -> bool| -> Vec<u64> {
            nodes.iter().filter(|(_, stored)| test(stored)).map(|(id, _)| *id).collect()
        };

        let equal = matching(&|stored| equals(stored, &value) == Some(true));
        prop_assert_eq!(equal_to(&value).map(seek).unwrap_or_default(), equal);

        for bound in BOUNDS {
            let expected = matching(&|stored| holds(bound, stored, &value));
            match compare_to(bound, &value) {
                Compared::Range(range) => prop_assert_eq!(seek(range), expected),
                Compared::Never => prop_assert_eq!(expected, Vec::<u64>::new()),
                Compared::Unserved => prop_assert!(matches!(value, Datum::List(_))),
            }
        }
    }
}

#[test]
fn a_pattern_property_map_reads_the_index_for_its_start_node() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph.rhizodb");
    let queries = [
        "MATCH (n:P {k: 1}) RETURN n",
        "MATCH (a)-[r:T {k: 2}]->(b) RETURN r",
        "UNWIND [0] AS i MATCH (n:P {k: i + 1}) RETURN n",
        "MATCH (n:P {k: toInteger('1')}) RETURN n",
        "MATCH (n:P {k: [x IN [0, 1] WHERE x > 0][0]}) RETURN n",
        "MATCH (a)-[r:T {k: CASE WHEN 1 < 2 THEN 2 END}]->(b) RETURN r",
        "MATCH (n:P {k: toInteger(rand()) + 1}) RETURN n",
        "MATCH (n:P {k: localtime.realtime().hour * 0 + 1}) RETURN n",
        "MATCH (n:P {k: CASE WHEN exists { MATCH (m) } THEN 1 END}) RETURN n",
        "MERGE (n:P {k: 1}) RETURN n",
    ];
    let count =
        |db: &Database, query: &str| db.execute(query, &Parameters::new()).unwrap().rows().len();
    {
        let db = Database::open(&path).unwrap();
        db.execute("INSERT (:P {k: 1})-[:T {k: 2}]->(:P)", &Parameters::new())
            .unwrap();
        db.create_property_index("p", PublicTarget::Label("P"), "k")
            .unwrap();
        db.create_property_index("t", PublicTarget::RelationshipType("T"), "k")
            .unwrap();
        assert_eq!(queries.map(|query| count(&db, query)), [1; 10]);
    }
    {
        let storage = open_path(&path, EngineOptions::default()).unwrap();
        let mut write = storage.begin_write().unwrap();
        for (key, _) in write.scan_prefix(Table::PropertyEntries, &[]).unwrap() {
            write.delete(Table::PropertyEntries, &key).unwrap();
        }
        write.commit().unwrap();
    }
    let db = Database::open(&path).unwrap();
    assert_eq!(
        queries.map(|query| count(&db, query)),
        [0, 0, 0, 0, 0, 0, 1, 1, 1, 1],
        "a value that can change between candidates, as rand() or a subquery may, is not looked up"
    );
    assert_eq!(count(&db, "MATCH (n:P) RETURN n"), 3);
}

#[test]
fn a_lookup_by_a_pattern_map_entry_tests_only_the_other_conditions_on_what_it_finds() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph.rhizodb");
    {
        let db = Database::open(&path).unwrap();
        db.execute(
            "INSERT (:P {k: 1, j: 1}), (:P {k: 2, j: 1})",
            &Parameters::new(),
        )
        .unwrap();
        db.create_property_index("p", PublicTarget::Label("P"), "k")
            .unwrap();
    }
    {
        let storage = open_path(&path, EngineOptions::default()).unwrap();
        let mut write = storage.begin_write().unwrap();
        let entries = write.scan_prefix(Table::PropertyEntries, &[]).unwrap();
        let (first, value) = entries.first().unwrap().clone();
        let mut planted = first.clone();
        let at = planted.len() - 8;
        planted[at..].copy_from_slice(&1u64.to_be_bytes());
        assert_ne!(planted, first);
        write.put(Table::PropertyEntries, &planted, &value).unwrap();
        write.commit().unwrap();
    }
    let db = Database::open(&path).unwrap();
    let ids = |query: &str| -> Vec<Value> {
        db.execute(query, &Parameters::new())
            .unwrap()
            .rows()
            .iter()
            .map(|row| row[0].clone())
            .collect()
    };
    let both = vec![Value::Integer(0), Value::Integer(1)];
    assert_eq!(ids("MATCH (n:P {k: 1}) RETURN id(n) ORDER BY id(n)"), both);
    assert_eq!(
        ids("MATCH (n:P {k: 1, j: 1}) RETURN id(n) ORDER BY id(n)"),
        both
    );
    assert_eq!(
        ids("MATCH (n:P {k: 1, j: 2}) RETURN id(n)"),
        Vec::<Value>::new()
    );
    assert_eq!(
        ids("MATCH (n:P {k: 1}) WHERE n.k = 1 RETURN id(n)"),
        vec![Value::Integer(0)]
    );
    assert_eq!(
        ids("MATCH (n:P) WHERE n.k = 1 RETURN id(n)"),
        vec![Value::Integer(0)]
    );
    assert_eq!(
        ids("MATCH (n:P {k: 2, k: 1}) RETURN id(n) ORDER BY id(n)"),
        both,
        "a repeated key looks up the entry the map holds, its last"
    );
}
