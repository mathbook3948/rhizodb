mod support;

use proptest::prelude::*;
use rhizodb::Database;
use rhizodb::ErrorKind;
use rhizodb::IndexTarget;
use rhizodb::Parameters;
use rhizodb::Value;
use rhizodb::VectorMetric;
use support::*;

fn indexed_db() -> Database {
    let db = db();
    db.create_property_index("p_k", IndexTarget::Label("P"), "k")
        .unwrap();
    db.create_property_index("t_k", IndexTarget::RelationshipType("T"), "k")
        .unwrap();
    db
}

fn with(parameters: &[(&str, Value)]) -> Parameters {
    parameters
        .iter()
        .map(|(name, value)| ((*name).to_owned(), value.clone()))
        .collect()
}

fn rows(db: &Database, query: &str, parameters: &Parameters) -> Vec<Vec<Value>> {
    match db.execute(query, parameters) {
        Ok(result) => result.rows().to_vec(),
        Err(error) => panic!("{query}: {error} ({:?})", error.kind()),
    }
}

fn ids(db: &Database, query: &str, value: Value) -> Vec<i64> {
    ints(&rows(db, query, &with(&[("v", value)])))
}

const BY_MAP: &str = "MATCH (n:P {k: $v}) RETURN id(n) ORDER BY id(n)";
const BY_WHERE: &str = "MATCH (n:P) WHERE n.k = $v RETURN id(n) ORDER BY id(n)";

fn trapped_db(indexed: bool) -> Database {
    let db = if indexed { indexed_db() } else { db() };
    run(
        &db,
        "INSERT (:P {k: 1, trap: 1}), (:P {k: 2, trap: 'x'}), (:P {k: 3, trap: 'x'}), \
         (:P {k: 1.5, trap: 1})-[:T {k: 7, trap: 1}]->(:P {k: 4, trap: 'x'})-[:T {k: 8, trap: 'x'}]->(:P {k: 4, trap: 'x'})",
    );
    db
}

#[test]
fn a_lookup_reads_only_the_elements_the_index_finds() {
    let served = [
        "MATCH (n:P) WHERE n.k = 1 AND n.trap - 1 = 0 RETURN id(n)",
        "MATCH (n:P) WHERE 1 = n.k AND n.trap - 1 = 0 RETURN id(n)",
        "MATCH (n:P) WHERE n.k IN [1, 1.5] AND n.trap - 1 = 0 RETURN id(n)",
        "MATCH (n:P) WHERE n.k < 2 AND n.trap - 1 = 0 RETURN id(n)",
        "MATCH (n:P) WHERE n.k >= 1 AND n.k <= 1.5 AND n.trap - 1 = 0 RETURN id(n)",
        "MATCH (n:P) WHERE 0 < n.k < 2 AND n.trap - 1 = 0 RETURN id(n)",
        "MATCH (n:P) WHERE n.k > 0.5 AND n.k < 1.75 AND n.trap - 1 = 0 RETURN id(n)",
        "MATCH ()-[r:T]->() WHERE r.k = 7 AND r.trap - 1 = 0 RETURN id(r)",
        "MATCH (n:P) WHERE id(n) = 0 AND n.trap - 1 = 0 RETURN id(n)",
        "MATCH (n) WHERE id(n) IN [0, 3.0] AND n.trap - 1 = 0 RETURN id(n)",
        "MATCH ()-[r]->() WHERE id(r) = 0 AND r.trap - 1 = 0 RETURN id(r)",
        "UNWIND [0] AS i MATCH (n:P) WHERE n.k = i + 1 AND n.trap - 1 = 0 RETURN id(n)",
        "MATCH (n:P) WHERE n.k = toInteger('1') AND n.trap - 1 = 0 RETURN id(n)",
        "MATCH (n:P) WHERE n.k = [1, 2][0] AND n.trap - 1 = 0 RETURN id(n)",
        "MATCH (n:P) WHERE n.k = CASE WHEN 1 < 2 THEN 1 END AND n.trap - 1 = 0 RETURN id(n)",
        "MATCH (n:P) WHERE n.k IN [x IN range(0, 3) WHERE x = 1] AND n.trap - 1 = 0 RETURN id(n)",
        "MATCH (n:P) WHERE n.k < -(-2) AND n.trap - 1 = 0 RETURN id(n)",
        "MATCH (m:P {trap: 1})-[:T]->(:P) MATCH (n:P) WHERE n.k = m.k - 0.5 AND n.trap - 1 = 0 RETURN id(n)",
    ];
    let indexed = trapped_db(true);
    let plain = trapped_db(false);
    for query in served {
        assert!(!run(&indexed, query).is_empty(), "{query}");
        if !query.contains("WHERE id(") {
            assert!(plain.execute(query, &Parameters::new()).is_err(), "{query}");
        }
    }
    let scanned = [
        "MATCH (n:P) WHERE n.k <> 2 AND n.trap - 1 = 0 RETURN id(n)",
        "MATCH (n:P) WHERE n.k + 0 = 1 AND n.trap - 1 = 0 RETURN id(n)",
        "MATCH (n:P) WHERE n.k = 1 OR n.trap - 1 = 0 RETURN id(n)",
        "MATCH (n:P) WHERE n.k = n.trap AND n.trap - 1 = 0 RETURN id(n)",
        "MATCH (n:P) WHERE n.k = toInteger(rand()) + 1 AND n.trap - 1 = 0 RETURN id(n)",
        "MATCH (n:P) WHERE n.k = 1 + localtime.realtime().hour * 0 AND n.trap - 1 = 0 RETURN id(n)",
        "MATCH (n:P) WHERE n.k = CASE WHEN exists { MATCH (m) } THEN 1 END AND n.trap - 1 = 0 RETURN id(n)",
        "MATCH (n:P) WHERE n.k = size([(m)-[:T]->() | m]) - 1 AND n.trap - 1 = 0 RETURN id(n)",
    ];
    for query in scanned {
        assert!(
            indexed.execute(query, &Parameters::new()).is_err(),
            "{query}"
        );
    }
}

#[test]
fn a_dropped_index_no_longer_serves_lookups() {
    let db = trapped_db(true);
    let query = "MATCH (n:P) WHERE n.k = 1 AND n.trap - 1 = 0 RETURN id(n)";
    assert_eq!(ints(&run(&db, query)), [0]);
    assert!(db.drop_index("p_k").unwrap());
    assert!(db.execute(query, &Parameters::new()).is_err());
}

#[test]
fn a_read_transaction_uses_the_indexes_of_its_snapshot() {
    let db = trapped_db(false);
    let query = "MATCH (n:P) WHERE n.k = 1 AND n.trap - 1 = 0 RETURN id(n)";
    let before_create = db.begin_read().unwrap();
    db.create_property_index("p_k", IndexTarget::Label("P"), "k")
        .unwrap();
    assert!(before_create.execute(query, &Parameters::new()).is_err());
    assert_eq!(ints(&run(&db, query)), [0]);
    let before_drop = db.begin_read().unwrap();
    assert!(db.drop_index("p_k").unwrap());
    let result = before_drop.execute(query, &Parameters::new()).unwrap();
    assert_eq!(ints(result.rows()), [0]);
    assert!(db.execute(query, &Parameters::new()).is_err());
}

#[test]
fn numbers_are_found_by_value_whether_integer_or_float() {
    let db = indexed_db();
    run(
        &db,
        "INSERT (:P {k: 1}), (:P {k: 1.0}), (:P {k: 1.5}), (:P {k: -0.0}), (:P {k: 0}), \
         (:P {k: 9007199254740993}), (:P {k: 9007199254740992.0}), (:P {k: 0.0 / 0.0}), (:P {k: '1'})",
    );
    for query in [BY_MAP, BY_WHERE] {
        assert_eq!(ids(&db, query, int(1)), [0, 1]);
        assert_eq!(ids(&db, query, float(1.0)), [0, 1]);
        assert_eq!(ids(&db, query, float(0.0)), [3, 4]);
        assert_eq!(ids(&db, query, int(9_007_199_254_740_993)), [5]);
        assert_eq!(ids(&db, query, int(9_007_199_254_740_992)), [6]);
        assert_eq!(ids(&db, query, float(f64::NAN)), Vec::<i64>::new());
        assert_eq!(ids(&db, query, Value::Null), Vec::<i64>::new());
        assert_eq!(ids(&db, query, string("1")), [8]);
    }
    let range = "MATCH (n:P) WHERE n.k > $v RETURN id(n) ORDER BY id(n)";
    assert_eq!(ids(&db, range, int(1)), [2, 5, 6]);
    assert_eq!(ids(&db, range, float(f64::NAN)), Vec::<i64>::new());
    assert_eq!(ids(&db, range, string("")), [8]);
}

#[test]
fn lists_strings_booleans_and_temporals_are_found_by_value() {
    let db = indexed_db();
    run(
        &db,
        "INSERT (:P {k: [1, 'a']}), (:P {k: [1.0, 'a']}), (:P {k: [1, 'a', 2]}), (:P {k: 'a'}), \
         (:P {k: 'ab'}), (:P {k: true}), \
         (:P {k: time('10:00+01:00')}), (:P {k: time('09:00Z')}), (:P {k: date('2024-01-02')})",
    );
    assert_eq!(ids(&db, BY_MAP, list([float(1.0), string("a")])), [0, 1]);
    assert_eq!(ids(&db, BY_WHERE, list([int(1), string("a")])), [0, 1]);
    assert_eq!(
        ids(&db, BY_MAP, list([int(1), Value::Null])),
        Vec::<i64>::new()
    );
    assert_eq!(ids(&db, BY_MAP, string("a")), [3]);
    assert_eq!(ids(&db, BY_MAP, boolean(true)), [5]);
    let between = "MATCH (n:P) WHERE n.k >= 'a' AND n.k < 'b' RETURN id(n) ORDER BY id(n)";
    assert_eq!(ints(&run(&db, between)), [3, 4]);
    let times = "MATCH (n:P) WHERE n.k <= time('09:00Z') RETURN id(n) ORDER BY id(n)";
    assert_eq!(ints(&run(&db, times)), [6, 7]);
    let same_instant = "MATCH (n:P) WHERE n.k = time('09:00Z') RETURN id(n)";
    assert_eq!(ints(&run(&db, same_instant)), [7]);
    let lists = "MATCH (n:P) WHERE n.k < [1, 'b'] RETURN id(n) ORDER BY id(n)";
    assert_eq!(ints(&run(&db, lists)), [0, 1, 2]);
}

#[test]
fn the_index_follows_property_and_label_writes_and_deletes() {
    let db = indexed_db();
    run(
        &db,
        "INSERT (:P {k: 1}), (:P {k: 1}), (:P {k: 1})-[:T {k: 5}]->(:P {k: 2})",
    );
    run(&db, "MATCH (n) WHERE id(n) = 0 SET n.k = 9");
    run(&db, "MATCH (n) WHERE id(n) = 1 REMOVE n:P");
    run(&db, "MATCH (n) WHERE id(n) = 2 SET n.k = null");
    assert_eq!(ids(&db, BY_MAP, int(1)), Vec::<i64>::new());
    assert_eq!(ids(&db, BY_WHERE, int(9)), [0]);
    run(&db, "MATCH (n) WHERE id(n) = 1 SET n:P");
    assert_eq!(ids(&db, BY_MAP, int(1)), [1]);
    let by_relationship = "MATCH (a)-[r:T {k: $v}]->(b) RETURN id(a) ORDER BY id(a)";
    assert_eq!(ids(&db, by_relationship, int(5)), [2]);
    run(&db, "MATCH ()-[r:T]->() SET r.k = 6");
    assert_eq!(ids(&db, by_relationship, int(5)), Vec::<i64>::new());
    assert_eq!(ids(&db, by_relationship, int(6)), [2]);
    run(&db, "MATCH (n) WHERE id(n) = 2 DETACH DELETE n");
    assert_eq!(ids(&db, by_relationship, int(6)), Vec::<i64>::new());
    run(&db, "MATCH (n:P {k: 1}) DELETE n");
    assert_eq!(ids(&db, BY_MAP, int(1)), Vec::<i64>::new());
    assert_eq!(ids(&db, BY_MAP, int(2)), [3]);
}

#[test]
fn a_rolled_back_transaction_leaves_the_index_unchanged() {
    let db = indexed_db();
    run(&db, "INSERT (:P {k: 1})");
    let mut write = db.begin_write().unwrap();
    write
        .execute("INSERT (:P {k: 2})", &Parameters::new())
        .unwrap();
    write
        .execute("MATCH (n:P {k: 1}) SET n.k = 3", &Parameters::new())
        .unwrap();
    let seen = write
        .execute(
            "MATCH (n:P) WHERE n.k IN [2, 3] RETURN id(n) ORDER BY id(n)",
            &Parameters::new(),
        )
        .unwrap();
    assert_eq!(ints(seen.rows()), [0, 1]);
    write.rollback().unwrap();
    assert_eq!(ids(&db, BY_MAP, int(1)), [0]);
    assert_eq!(ids(&db, BY_MAP, int(2)), Vec::<i64>::new());
    assert_eq!(ids(&db, BY_MAP, int(3)), Vec::<i64>::new());
}

#[test]
fn creating_an_index_fills_it_and_it_survives_reopening_the_file() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph.redb");
    {
        let db = Database::open(&path).unwrap();
        run(
            &db,
            "INSERT (:P {k: 'kept', trap: 1}), (:P {k: 'other', trap: 'x'})",
        );
        db.create_property_index("p_k", IndexTarget::Label("P"), "k")
            .unwrap();
    }
    let db = Database::open(&path).unwrap();
    let query = "MATCH (n:P) WHERE n.k = 'kept' AND n.trap - 1 = 0 RETURN n.k";
    assert_eq!(run(&db, query), [vec![string("kept")]]);
}

#[test]
fn a_merge_finds_its_node_through_the_index() {
    let db = indexed_db();
    run(&db, "MERGE (:P {k: 1})");
    run(&db, "MERGE (:P {k: 1.0})");
    assert_eq!(ints(&run(&db, "MATCH (n:P) RETURN count(n)")), [1]);
}

#[test]
fn property_index_names_follow_the_rules_of_the_other_kinds() {
    let db = db();
    db.create_property_index("by_k", IndexTarget::Label("P"), "k")
        .unwrap();
    db.create_property_index("by_k", IndexTarget::Label("P"), "k")
        .unwrap();
    let kind = |result: Result<(), rhizodb::Error>| result.unwrap_err().kind();
    assert_eq!(
        kind(db.create_property_index("by_k", IndexTarget::Label("P"), "j")),
        ErrorKind::IndexAlreadyExists
    );
    assert_eq!(
        kind(db.create_keyword_index("by_k", IndexTarget::Label("P"), &["k"])),
        ErrorKind::IndexAlreadyExists
    );
    assert_eq!(
        kind(db.create_vector_index(
            "by_k",
            IndexTarget::Label("P"),
            "k",
            2,
            VectorMetric::Cosine
        )),
        ErrorKind::IndexAlreadyExists
    );
    assert_eq!(
        kind(db.create_property_index("", IndexTarget::Label("P"), "k")),
        ErrorKind::InvalidIndexDefinition
    );
    let search = "CALL db.index.keyword.queryNodes('by_k', 'x', 1) YIELD node RETURN node";
    assert_eq!(error_kind(&db, search), ErrorKind::IndexNotFound);
    assert!(db.drop_index("by_k").unwrap());
    assert!(!db.drop_index("by_k").unwrap());
}

#[derive(Clone, Debug)]
enum Write {
    Node { labelled: bool, value: Value },
    Link { from: i64, to: i64, value: Value },
    Set { node: i64, value: Value },
    Remove { node: i64 },
    Unlabel { node: i64 },
    Delete { node: i64 },
}

fn value() -> impl Strategy<Value = Value> + Clone {
    let scalar = prop_oneof![
        (-3i64..3).prop_map(Value::Integer),
        prop::sample::select(vec![-1.5, -0.0, 0.5, 1.0, 2.0, f64::NAN, f64::INFINITY])
            .prop_map(Value::Float),
        "[ab]{0,2}".prop_map(Value::String),
        any::<bool>().prop_map(Value::Boolean),
    ];
    prop_oneof![
        4 => scalar.clone(),
        1 => prop::collection::vec(scalar, 0..3).prop_map(Value::List),
    ]
}

fn write() -> impl Strategy<Value = Write> {
    let node = 0i64..8;
    prop_oneof![
        4 => (any::<bool>(), value()).prop_map(|(labelled, value)| Write::Node { labelled, value }),
        2 => (node.clone(), node.clone(), value()).prop_map(|(from, to, value)| Write::Link { from, to, value }),
        2 => (node.clone(), value()).prop_map(|(node, value)| Write::Set { node, value }),
        1 => node.clone().prop_map(|node| Write::Remove { node }),
        1 => node.clone().prop_map(|node| Write::Unlabel { node }),
        1 => node.prop_map(|node| Write::Delete { node }),
    ]
}

fn apply(db: &Database, write: &Write) {
    let (query, parameters) = match write {
        Write::Node { labelled, value } => (
            if *labelled {
                "INSERT (:P {k: $v})"
            } else {
                "INSERT (:Q {k: $v})"
            },
            with(&[("v", value.clone())]),
        ),
        Write::Link { from, to, value } => (
            "MATCH (a), (b) WHERE id(a) = $a AND id(b) = $b INSERT (a)-[:T {k: $v}]->(b)",
            with(&[("a", int(*from)), ("b", int(*to)), ("v", value.clone())]),
        ),
        Write::Set { node, value } => (
            "MATCH (n) WHERE id(n) = $a SET n.k = $v",
            with(&[("a", int(*node)), ("v", value.clone())]),
        ),
        Write::Remove { node } => (
            "MATCH (n) WHERE id(n) = $a REMOVE n.k",
            with(&[("a", int(*node))]),
        ),
        Write::Unlabel { node } => (
            "MATCH (n) WHERE id(n) = $a REMOVE n:P",
            with(&[("a", int(*node))]),
        ),
        Write::Delete { node } => (
            "MATCH (n) WHERE id(n) = $a DETACH DELETE n",
            with(&[("a", int(*node))]),
        ),
    };
    rows(db, query, &parameters);
}

const LOOKUPS: [&str; 15] = [
    "MATCH (n:P {k: $v}) RETURN id(n) AS id",
    "MATCH (n:P {k: $v}) WHERE n.k = $w RETURN id(n) AS id",
    "MATCH (n:P {k: $v, k: $w}) RETURN id(n) AS id",
    "MATCH (n:P {k: $v})-[r:T]->(m:P {k: $w}) RETURN id(r) AS id",
    "MATCH (n:P) WHERE n.k = $v RETURN id(n) AS id",
    "MATCH (n:P) WHERE n.k IN [$v, $w] RETURN id(n) AS id",
    "MATCH (n:P) WHERE n.k < $v RETURN id(n) AS id",
    "MATCH (n:P) WHERE n.k <= $v RETURN id(n) AS id",
    "MATCH (n:P) WHERE $v < n.k RETURN id(n) AS id",
    "MATCH (n:P) WHERE n.k >= $v RETURN id(n) AS id",
    "MATCH (n:P) WHERE n.k > $v AND n.k <= $w RETURN id(n) AS id",
    "MATCH (n:P) WHERE $v <= n.k < $w RETURN id(n) AS id",
    "MATCH (a)-[r:T {k: $v}]->(b) RETURN id(r) AS id",
    "MATCH (a)<-[r:T]-(b) WHERE r.k >= $v RETURN id(r) AS id",
    "MATCH (n) WHERE id(n) IN [$v, $w] RETURN id(n) AS id",
];

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    #[ignore = "left out of mise run check, run by mise run random"]
    fn a_lookup_finds_what_a_scan_finds_after_any_writes(
        writes in prop::collection::vec(write(), 0..30),
        lookups in prop::collection::vec((0..LOOKUPS.len(), value(), value()), 1..12),
    ) {
        let indexed = indexed_db();
        let plain = db();
        for write in &writes {
            apply(&indexed, write);
            apply(&plain, write);
        }
        for (lookup, first, second) in lookups {
            let query = format!("{} ORDER BY id", LOOKUPS[lookup]);
            let scan = query.replace("id(n) IN", "id(n) + 0 IN");
            let parameters = with(&[("v", first), ("w", second)]);
            prop_assert_eq!(
                rows(&indexed, &query, &parameters),
                rows(&plain, &scan, &parameters),
                "{}",
                query
            );
        }
    }
}

const EXPRESSION_LOOKUPS: [&str; 12] = [
    "UNWIND [$v, $w] AS x MATCH (n:P {k: x}) RETURN id(n) AS id",
    "UNWIND range(-2, 2) AS i MATCH (n:P {k: i + 1}) RETURN id(n) AS id",
    "MATCH (n:P {k: coalesce($v, $w)}) RETURN id(n) AS id",
    "MATCH (n:P {k: $v + 0}) RETURN id(n) AS id",
    "MATCH (n:P) WHERE n.k = toInteger($v) RETURN id(n) AS id",
    "MATCH (n:P) WHERE n.k = [$v, $w][1] RETURN id(n) AS id",
    "MATCH (n:P) WHERE n.k = $v[0] RETURN id(n) AS id",
    "MATCH (n:P) WHERE n.k IN [x IN [$v, $w] WHERE x IS NOT NULL | x] RETURN id(n) AS id",
    "MATCH (n:P) WHERE n.k < CASE WHEN $v < $w THEN $v ELSE $w END RETURN id(n) AS id",
    "MATCH (n:P) WHERE n.k >= -$v RETURN id(n) AS id",
    "MATCH (m:P) MATCH (n:P {k: m.k}) RETURN id(n) AS id",
    "MATCH (m:P)-[r:T]->() MATCH (n:P) WHERE n.k = r.k + $v RETURN id(n) AS id",
];

fn outcome(
    db: &Database,
    query: &str,
    parameters: &Parameters,
) -> Result<Vec<Vec<Value>>, ErrorKind> {
    db.execute(query, parameters)
        .map(|result| result.rows().to_vec())
        .map_err(|error| error.kind())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    #[ignore = "left out of mise run check, run by mise run random"]
    fn a_lookup_by_an_expression_finds_what_a_scan_finds(
        writes in prop::collection::vec(write(), 0..30),
        lookups in prop::collection::vec((0..EXPRESSION_LOOKUPS.len(), value(), value()), 1..12),
    ) {
        let indexed = indexed_db();
        let plain = db();
        for write in &writes {
            apply(&indexed, write);
            apply(&plain, write);
        }
        for (lookup, first, second) in lookups {
            let query = format!("{} ORDER BY id", EXPRESSION_LOOKUPS[lookup]);
            let parameters = with(&[("v", first), ("w", second)]);
            prop_assert_eq!(
                outcome(&indexed, &query, &parameters),
                outcome(&plain, &query, &parameters),
                "{}",
                query
            );
        }
    }
}

#[test]
fn an_id_lookup_answers_as_a_lookup_that_tests_its_condition_again() {
    let db = trapped_db(false);
    let shapes = [
        "UNWIND [VALUE] AS v MATCH (n) WHERE ID = v RETURN id(n)",
        "UNWIND [VALUE] AS v OPTIONAL MATCH (n) WHERE ID = v RETURN id(n)",
        "UNWIND [VALUE] AS v MATCH (n:P) WHERE v = ID AND n.k > 1 RETURN id(n), n.k",
        "UNWIND [VALUE] AS v MATCH (n)-[r:T]->(m) WHERE ID = v RETURN id(r), id(m)",
        "UNWIND [VALUE] AS v OPTIONAL MATCH (n {k: 4}) WHERE ID = v RETURN id(n)",
        "UNWIND [VALUE] AS v MATCH p = (n) WHERE ID = v RETURN length(p)",
        "UNWIND [VALUE] AS v MATCH (n) WHERE n.k > 0 AND ID = v AND n.k < 10 RETURN id(n)",
        "UNWIND [VALUE] AS v MATCH (n) WHERE n.trap AND ID = v RETURN id(n)",
        "UNWIND [VALUE] AS v MATCH (n) WHERE ID = v AND n.trap - 1 = 0 RETURN id(n)",
        "UNWIND [VALUE] AS v MATCH (n) WHERE ID = v MATCH (n)-[:T]->(m) RETURN id(m)",
    ];
    let values = ["0", "3", "3.0", "99", "-1", "null", "'x'", "2.5", "[0]"];
    let answer = |query: &str| match db.execute(query, &Parameters::new()) {
        Ok(result) => format!("{:?}", result.rows()),
        Err(error) => format!("error {:?}: {error}", error.kind()),
    };
    for shape in shapes {
        for value in values {
            let query = shape.replace("VALUE", value);
            let sought = query.replace("ID", "id(n)");
            let twice = query
                .replace("ID = v", "(id(n) = v AND id(n) = v)")
                .replace("v = ID", "(v = id(n) AND v = id(n))");
            assert_eq!(answer(&sought), answer(&twice), "{sought}");
        }
    }
}
