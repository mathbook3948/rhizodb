mod support;

use rhizodb::ErrorKind;
use rhizodb::Value;
use support::*;

fn element_names(db: &rhizodb::Database) -> Vec<Vec<Value>> {
    let mut rows = run(
        db,
        "MATCH (n) RETURN 'node', labels(n), keys(n), keys(properties(n)) ORDER BY id(n)",
    );
    rows.extend(run(
        db,
        "MATCH ()-[r]->() RETURN 'relationship', [type(r)], keys(r), keys(properties(r))",
    ));
    rows
}

fn in_tx(tx: &mut rhizodb::WriteTransaction<'_>, query: &str) -> Vec<Vec<Value>> {
    tx.execute(query, &rhizodb::Parameters::new())
        .unwrap()
        .rows()
        .to_vec()
}

#[test]
fn names_of_a_token_a_rolled_back_transaction_created_never_reach_later_readers() {
    let db = db();
    let returned = "MATCH (a)-[r]->() RETURN labels(a), keys(a), type(r), keys(r)";
    let mut ghost = db.begin_write().unwrap();
    in_tx(
        &mut ghost,
        "CREATE (:Ghost {ghostKey: 1})-[:GHOST_TYPE {ghostEdge: 2}]->(:Ghost)",
    );
    assert_eq!(
        in_tx(&mut ghost, returned),
        [vec![
            list([string("Ghost")]),
            list([string("ghostKey")]),
            string("GHOST_TYPE"),
            list([string("ghostEdge")])
        ]]
    );
    assert!(element_names(&db).is_empty());
    ghost.rollback().unwrap();

    run(
        &db,
        "CREATE (:Real {realKey: 1})-[:REAL_TYPE {realEdge: 2}]->(:Real)",
    );
    let real = vec![
        list([string("Real")]),
        list([string("realKey")]),
        string("REAL_TYPE"),
        list([string("realEdge")]),
    ];
    assert_eq!(run(&db, returned), std::slice::from_ref(&real));
    assert_eq!(
        element_names(&db),
        [
            vec![
                string("node"),
                list([string("Real")]),
                list([string("realKey")]),
                list([string("realKey")])
            ],
            vec![string("node"), list([string("Real")]), list([]), list([])],
            vec![
                string("relationship"),
                list([string("REAL_TYPE")]),
                list([string("realEdge")]),
                list([string("realEdge")])
            ],
        ]
    );
    assert_eq!(single(&db, "MATCH (n:Ghost) RETURN count(n)"), int(0));
    let mut write = db.begin_write().unwrap();
    assert_eq!(in_tx(&mut write, returned), [real]);
    write.rollback().unwrap();
}

#[test]
fn labels_keys_and_type_follow_every_change_a_transaction_makes() {
    let db = db();
    run(
        &db,
        "UNWIND range(0, 299) AS i CREATE (n:N {i: i}) \
         SET n.a = CASE WHEN i % 2 = 0 THEN i END, \
             n.b = CASE WHEN i % 3 = 0 THEN 'text ' + toString(i) END, \
             n.c = CASE WHEN i % 5 = 0 THEN [x IN range(1, 64) | toFloat(x)] END",
    );
    run(
        &db,
        "MATCH (a:N), (b:N) WHERE b.i = a.i + 1 AND a.i % 7 = 0 \
         CREATE (a)-[:R {w: a.i, z: CASE WHEN a.i % 2 = 0 THEN 'z' END}]->(b)",
    );
    let query = "MATCH (n) RETURN keys(n) = keys(properties(n)), keys(n), labels(n) ORDER BY n.i";
    let agreed = |rows: Vec<Vec<Value>>| {
        for row in &rows {
            assert_eq!(row[0], boolean(true), "{row:?}");
        }
        rows
    };
    let names = |keys: &[&str], labels: &[&str]| {
        vec![
            boolean(true),
            list(keys.iter().map(|key| string(key))),
            list(labels.iter().map(|label| string(label))),
        ]
    };
    let before = agreed(run(&db, query));
    assert_eq!(before[30], names(&["a", "b", "c", "i"], &["N"]));
    assert_eq!(before[7], names(&["i"], &["N"]));

    run(&db, "MATCH (n:N) WHERE n.i < 10 REMOVE n.a SET n.d = 1");
    run(&db, "MATCH (n:N) WHERE n.i >= 100 REMOVE n.b SET n:M");
    let after = agreed(run(&db, query));
    assert_eq!(after[6], names(&["b", "d", "i"], &["N"]));
    assert_eq!(after[150], names(&["a", "c", "i"], &["N", "M"]));

    let mut tx = db.begin_write().unwrap();
    in_tx(
        &mut tx,
        "MATCH (n:N) WHERE n.i = 150 REMOVE n.c, n:M SET n.e = 'new'",
    );
    in_tx(&mut tx, "CREATE (:Fresh {i: 1000, f: 1})");
    in_tx(&mut tx, "MATCH ()-[r:R {w: 14}]->() REMOVE r.z SET r.y = 1");
    let inside = agreed(in_tx(&mut tx, query));
    assert_eq!(inside[150], names(&["a", "e", "i"], &["N"]));
    assert_eq!(inside[300], names(&["f", "i"], &["Fresh"]));
    assert_eq!(
        in_tx(
            &mut tx,
            "MATCH ()-[r]->() WHERE r.w IN [0, 7, 14] RETURN type(r), keys(r) ORDER BY r.w"
        ),
        [
            vec![string("R"), list([string("w"), string("z")])],
            vec![string("R"), list([string("w")])],
            vec![string("R"), list([string("w"), string("y")])],
        ]
    );
    tx.rollback().unwrap();
    assert_eq!(agreed(run(&db, query)), after);
}

fn row(db: &rhizodb::Database, query: &str) -> Vec<Value> {
    let mut rows = run(db, query);
    assert_eq!(rows.len(), 1, "{query}");
    rows.remove(0)
}

#[test]
fn gql_string_functions_measure_case_and_trim_text() {
    let db = db();
    assert_eq!(
        row(
            &db,
            "RETURN upper('ab'), lower('AB'), char_length('héllo'), character_length(''), \
             octet_length('é'), byte_length('ab')"
        ),
        [string("AB"), string("ab"), int(5), int(0), int(2), int(2)]
    );
    assert_eq!(
        row(
            &db,
            "RETURN trim(LEADING 'x' FROM 'xxaxx'), trim(TRAILING 'x' FROM 'xxaxx'), \
             trim(BOTH 'x' FROM 'xxaxx'), trim('x' FROM 'xxaxx'), trim(BOTH FROM '  a  '), trim(' a ')"
        ),
        [
            string("axx"),
            string("xxa"),
            string("a"),
            string("a"),
            string("a"),
            string("a")
        ]
    );
    assert_eq!(
        row(
            &db,
            "RETURN btrim('xyaxy', 'xy'), ltrim('xxa', 'x'), rtrim('axx', 'x'), ltrim('  a'), \
             rtrim(null, 'x'), ltrim('a', null)"
        ),
        [
            string("a"),
            string("a"),
            string("a"),
            string("a"),
            Value::Null,
            Value::Null
        ]
    );
}

#[test]
fn trim_still_takes_a_variable_named_like_a_trim_keyword() {
    let db = db();
    assert_eq!(
        single(&db, "WITH ' a ' AS leading RETURN trim(leading) AS t"),
        string("a")
    );
    assert_eq!(
        row(
            &db,
            "WITH {name: ' b '} AS both, ' l' AS leading, 'x' AS from \
             RETURN trim(both.name), trim(leading + 'x'), trim(BOTH FROM ' c '), \
             trim(LEADING 'x' FROM 'xxd'), trim('y' FROM 'yey')"
        ),
        [
            string("b"),
            string("lx"),
            string("c"),
            string("d"),
            string("e")
        ]
    );
}

#[test]
fn gql_numeric_functions_compute_logs_powers_and_hyperbolics() {
    let db = db();
    assert_eq!(
        row(
            &db,
            "RETURN ln(1), log(2, 8), power(2, 3), mod(7, 3), mod(-7, 3), sinh(0), cosh(0), tanh(0)"
        ),
        [
            float(0.0),
            float(3.0),
            float(8.0),
            int(1),
            int(-1),
            float(0.0),
            float(1.0),
            float(0.0)
        ]
    );
    assert_eq!(
        single(&db, "RETURN power(2, 0.5) = sqrt(2) AS same"),
        boolean(true)
    );
    assert_eq!(
        single(&db, "RETURN cot(1) = 1 / tan(1) AS same"),
        boolean(true)
    );
    assert_eq!(single(&db, "RETURN power(null, 2) AS v"), Value::Null);
    assert_eq!(
        error_kind(&db, "RETURN power('a', 1) AS v"),
        ErrorKind::InvalidArgumentType
    );
}

#[test]
fn predicates_on_values_test_emptiness_and_nan() {
    let db = db();
    assert_eq!(
        row(
            &db,
            "RETURN isNaN(0.0 / 0.0), isNaN(1), isEmpty([]), isEmpty(''), isEmpty({a: 1}), \
             cardinality([1, 2])"
        ),
        [
            boolean(true),
            boolean(false),
            boolean(true),
            boolean(true),
            boolean(false),
            int(2)
        ]
    );
    assert_eq!(
        error_kind(&db, "RETURN cardinality('ab') AS v"),
        ErrorKind::InvalidArgumentType
    );
}

#[test]
fn nullif_gives_null_when_its_arguments_are_equal() {
    let db = db();
    assert_eq!(
        row(
            &db,
            "RETURN nullif(1, 1), nullif(1, 2), nullif(1, 1.0), nullif(null, 1), nullif(1, null)"
        ),
        [Value::Null, int(1), Value::Null, Value::Null, int(1)]
    );
}

#[test]
fn or_null_conversions_give_null_instead_of_failing() {
    let db = db();
    assert_eq!(
        row(
            &db,
            "RETURN toStringOrNull([1]), toStringOrNull(1), toIntegerOrNull('x'), toIntegerOrNull('12'), \
             toFloatOrNull({}), toBooleanOrNull('true')"
        ),
        [
            Value::Null,
            string("1"),
            Value::Null,
            int(12),
            Value::Null,
            boolean(true)
        ]
    );
}

#[test]
fn element_ids_and_path_length_read_graph_elements() {
    let db = db();
    let ids = row(
        &db,
        "INSERT (n:A)-[r:R]->(:B) RETURN elementId(n) = elementId(n), elementId(n) <> elementId(r)",
    );
    assert_eq!(ids, [boolean(true), boolean(true)]);
    assert_eq!(
        single(&db, "MATCH p = (:A)-->() RETURN path_length(p) AS l"),
        int(1)
    );
}

#[test]
fn a_simple_case_tests_its_value_with_lists_and_comparisons() {
    let db = db();
    let rows = run(
        &db,
        "UNWIND [0, 1, 2, 5, null] AS x RETURN CASE x WHEN 1, 2 THEN 'few' WHEN >= 5 THEN 'many' \
         WHEN IS NULL THEN 'none' ELSE 'other' END AS c",
    );
    assert_eq!(
        rows,
        [
            [string("other")],
            [string("few")],
            [string("few")],
            [string("many")],
            [string("none")]
        ]
    );
    assert_eq!(
        single(
            &db,
            "RETURN CASE 3 WHEN < 1, IS NOT NULL THEN 'yes' END AS c"
        ),
        string("yes")
    );
}

#[test]
fn a_simple_case_with_tests_evaluates_its_value_once() {
    let db = db();
    let rows = run(
        &db,
        "UNWIND range(1, 50) AS i WITH CASE rand() WHEN < 0.5 THEN 0 WHEN >= 0.5 THEN 1 END AS c \
         RETURN count(c) AS n",
    );
    assert_eq!(
        rows,
        [[int(50)]],
        "every row took one of two complementary branches"
    );
}

#[test]
fn order_by_places_nulls_first_or_last_as_asked() {
    let db = db();
    let order = |suffix: &str| {
        let rows = run(
            &db,
            &format!("UNWIND [3, null, 1] AS x RETURN x ORDER BY x {suffix}"),
        );
        rows.into_iter()
            .map(|row| row[0].clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(order(""), [int(1), int(3), Value::Null]);
    assert_eq!(order("DESC"), [Value::Null, int(3), int(1)]);
    assert_eq!(order("NULLS FIRST"), [Value::Null, int(1), int(3)]);
    assert_eq!(order("ASC NULLS LAST"), [int(1), int(3), Value::Null]);
    assert_eq!(order("DESC NULLS LAST"), [int(3), int(1), Value::Null]);
    assert_eq!(order("DESC NULLS FIRST"), [Value::Null, int(3), int(1)]);
}

#[test]
fn nulls_order_applies_to_a_column_scan_too() {
    let db = db();
    run(&db, "UNWIND [3, null, 1] AS x INSERT (:N {x: x})");
    let rows = run(
        &db,
        "MATCH (n:N) RETURN n.x AS x ORDER BY n.x NULLS FIRST LIMIT 2",
    );
    assert_eq!(rows, [[Value::Null], [int(1)]]);
    let rows = run(
        &db,
        "MATCH (n:N) RETURN n.x AS x ORDER BY n.x DESC NULLS LAST",
    );
    assert_eq!(rows, [[int(3)], [int(1)], [Value::Null]]);
}

#[test]
fn concatenation_joins_two_strings_or_two_lists() {
    let db = db();
    assert_eq!(
        row(&db, "RETURN 'a' || 'b' || 'c', [1] || [2, 3], 'a' || null"),
        [string("abc"), list([int(1), int(2), int(3)]), Value::Null]
    );
}

#[test]
fn concatenation_rejects_other_types_before_reading_data() {
    let db = db();
    for query in [
        "RETURN 'a' || 1 AS v",
        "RETURN [1] || 'a' AS v",
        "RETURN 1 || 2 AS v",
    ] {
        assert_eq!(
            error_kind(&db, query),
            ErrorKind::InvalidArgumentType,
            "{query}"
        );
    }
    let parameter = rhizodb::Parameters::from([("p".to_owned(), int(1))]);
    let kind = |query: &str| db.execute(query, &parameter).unwrap_err().kind();
    assert_eq!(kind("RETURN 'a' || $p AS v"), kind("RETURN 'a' - $p AS v"));
}

#[test]
fn a_single_pipe_still_ends_a_comprehension() {
    let db = db();
    assert_eq!(
        single(&db, "RETURN [x IN ['a'] | x || 'b'] AS l"),
        list([string("ab")])
    );
    assert_eq!(
        single(&db, "RETURN [x IN ['a'] WHERE x || 'b' = 'ab' | x] AS l"),
        list([string("a")])
    );
}

#[test]
fn random_uuid_gives_a_new_version_four_uuid_each_call() {
    let db = db();
    assert_eq!(
        run(
            &db,
            "RETURN randomUUID() =~ '[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}' AS shaped, \
             randomUUID() <> randomUUID() AS fresh"
        ),
        [[boolean(true), boolean(true)]]
    );
    assert_eq!(
        single(
            &db,
            "UNWIND range(1, 100) AS i WITH collect(randomUUID()) AS ids \
             RETURN size(ids) = size(COLLECT { UNWIND ids AS id RETURN DISTINCT id }) AS unique"
        ),
        boolean(true)
    );
}

#[test]
fn haversin_is_half_of_one_minus_the_cosine() {
    let db = db();
    assert_eq!(single(&db, "RETURN haversin(0) AS h"), float(0.0));
    assert_eq!(
        single(&db, "RETURN abs(haversin(pi()) - 1.0) < 1e-12 AS h"),
        boolean(true)
    );
    assert_eq!(single(&db, "RETURN haversin(null) AS h"), Value::Null);
}

#[test]
fn round_takes_a_precision_and_a_rounding_mode() {
    let db = db();
    let rows = run(
        &db,
        "RETURN round(1.25, 1), round(-1.25, 1), round(1.25, 1, 'HALF_EVEN'), round(1.35, 1, 'HALF_EVEN'), \
         round(1.25, 1, 'HALF_DOWN'), round(1.21, 1, 'UP'), round(1.29, 1, 'DOWN'), round(-1.21, 1, 'CEILING'), \
         round(-1.21, 1, 'FLOOR'), round(9.99, 1), round(5, 2), round(1.005, 2), round(2.5)",
    );
    assert_eq!(
        rows,
        [[
            float(1.3),
            float(-1.3),
            float(1.2),
            float(1.4),
            float(1.2),
            float(1.3),
            float(1.2),
            float(-1.2),
            float(-1.3),
            float(10.0),
            float(5.0),
            float(1.01),
            float(3.0)
        ]]
    );
    assert_eq!(
        error_kind(&db, "RETURN round(1.5, -1) AS r"),
        ErrorKind::NumberOutOfRange
    );
    assert_eq!(
        error_kind(&db, "RETURN round(1.5, 1, 'SIDEWAYS') AS r"),
        ErrorKind::InvalidArgumentValue
    );
    assert_eq!(
        error_kind(&db, "RETURN round(1.5, 1.5) AS r"),
        ErrorKind::InvalidArgumentType
    );
}

#[test]
fn timestamp_reads_the_statement_clock_in_milliseconds() {
    let db = db();
    assert_eq!(
        single(
            &db,
            "RETURN timestamp() = timestamp() AND timestamp() = datetime().epochMillis AS same"
        ),
        boolean(true)
    );
}

#[test]
fn ceiling_is_ceil() {
    let db = db();
    assert_eq!(single(&db, "RETURN ceiling(1.2) AS c"), float(2.0));
}

#[test]
fn list_conversions_give_null_for_items_they_cannot_convert() {
    let db = db();
    assert_eq!(
        run(
            &db,
            "RETURN toBooleanList(['true', 'x', null, false]), toIntegerList(['1', 2.5, 'x']), \
             toFloatList(['1.5', 2]), toStringList([1, true, 2.5]), toIntegerList(null)"
        ),
        [[
            list([boolean(true), Value::Null, Value::Null, boolean(false)]),
            list([int(1), int(2), Value::Null]),
            list([float(1.5), float(2.0)]),
            list([string("1"), string("true"), string("2.5")]),
            Value::Null
        ]]
    );
    assert_eq!(
        error_kind(&db, "RETURN toIntegerList(1) AS l"),
        ErrorKind::InvalidArgumentType
    );
}

#[test]
fn vector_similarities_follow_neo4j() {
    let db = db();
    assert_eq!(
        run(
            &db,
            "RETURN vector.similarity.cosine([1, 0], [2, 0]), vector.similarity.cosine([1, 0], [0, 1]), \
             vector.similarity.cosine([1, 0], [-1, 0]), vector.similarity.euclidean([1, 2], [1, 2]), \
             vector.similarity.euclidean([0, 0], [3, 4]), vector.similarity.cosine([0, 0], [1, 1]), \
             vector.similarity.cosine(null, [1]), vector.similarity.cosine([1, null], [1, 1])"
        ),
        [[
            float(1.0),
            float(0.5),
            float(0.0),
            float(1.0),
            float(1.0 / 26.0),
            Value::Null,
            Value::Null,
            Value::Null
        ]]
    );
    assert_eq!(
        error_kind(&db, "RETURN vector.similarity.cosine([1, 2], [1]) AS s"),
        ErrorKind::InvalidArgumentValue
    );
    assert_eq!(
        error_kind(
            &db,
            "UNWIND [['a']] AS v RETURN vector.similarity.euclidean(v, v) AS s"
        ),
        ErrorKind::InvalidArgumentValue
    );
}

#[test]
fn gql_current_time_functions_read_the_statement_clock() {
    let db = db();
    assert_eq!(
        run(
            &db,
            "RETURN CURRENT_DATE = date(), CURRENT_TIMESTAMP = datetime(), LOCAL_TIMESTAMP = localdatetime(), \
             current_time = time(), LOCAL_TIME = localtime()"
        ),
        [[
            boolean(true),
            boolean(true),
            boolean(true),
            boolean(true),
            boolean(true)
        ]]
    );
    assert_eq!(
        single(&db, "WITH 1 AS current_date RETURN current_date"),
        int(1)
    );
    assert_eq!(
        run(
            &db,
            "UNWIND [date('2000-01-01'), date('2001-01-01')] AS at \
             RETURN max(at) < CURRENT_DATE AS stale, max(at) AS last, \
             duration.between(max(at), CURRENT_DATE).years >= 25 AS old \
             ORDER BY duration.between(last, CURRENT_TIMESTAMP)"
        ),
        [[
            boolean(true),
            single(&db, "RETURN date('2001-01-01')"),
            boolean(true)
        ]]
    );
}
