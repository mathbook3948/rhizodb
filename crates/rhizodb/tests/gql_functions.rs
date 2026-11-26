mod support;

use rhizodb::ErrorKind;
use rhizodb::Value;
use support::*;

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
}
