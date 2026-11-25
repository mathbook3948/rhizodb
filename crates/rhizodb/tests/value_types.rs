mod support;

use rhizodb::ErrorKind;
use rhizodb::Value;
use support::*;

fn row(db: &rhizodb::Database, query: &str) -> Vec<Value> {
    let rows = run(db, query);
    assert_eq!(rows.len(), 1, "{query}");
    rows[0].clone()
}

fn booleans(values: &[bool]) -> Vec<Value> {
    values.iter().map(|value| boolean(*value)).collect()
}

#[test]
fn is_typed_tests_the_type_of_a_value() {
    let db = db();
    assert_eq!(
        row(
            &db,
            "RETURN 1 IS :: INTEGER, 1 IS TYPED INT, 1 IS NOT TYPED STRING, 1.5 IS :: INTEGER | FLOAT, \
             'a' IS :: ANY<STRING | BOOLEAN>, {a: 1} IS :: MAP, 1 IS :: ANY, 1 IS :: NULL"
        ),
        booleans(&[true, true, true, true, true, true, true, false])
    );
    run(&db, "INSERT (:N)-[:R]->(:N)");
    assert_eq!(
        row(
            &db,
            "MATCH p = (a)-[r]->(b) RETURN a IS :: NODE, r IS :: EDGE, p IS :: PATH, a IS :: RELATIONSHIP"
        ),
        booleans(&[true, true, true, false])
    );
    assert_eq!(
        row(
            &db,
            "RETURN date() IS :: DATE, localtime() IS :: LOCAL TIME, time() IS :: ZONED TIME, \
             localdatetime() IS :: TIMESTAMP WITHOUT TIME ZONE, datetime() IS :: ZONED DATETIME, \
             duration('P1D') IS :: DURATION, date() IS :: DURATION"
        ),
        booleans(&[true, true, true, true, true, true, false])
    );
}

#[test]
fn null_belongs_to_every_type_not_declared_not_null() {
    let db = db();
    assert_eq!(
        row(
            &db,
            "RETURN null IS :: INTEGER, null IS :: INTEGER NOT NULL, null IS :: NULL, null IS :: NOTHING, \
             null IS :: ANY NOT NULL, [1, null] IS :: LIST<INTEGER>, [1, null] IS :: LIST<INTEGER NOT NULL>, \
             [] IS :: LIST<NOTHING>, [1, 'a'] IS :: LIST<INTEGER>"
        ),
        booleans(&[true, false, true, false, false, true, false, true, false])
    );
}

#[test]
fn a_type_union_in_a_comprehension_filter_needs_parentheses() {
    let db = db();
    assert_eq!(
        single(
            &db,
            "RETURN [x IN [1, 'a', 2.5] WHERE (x IS :: INTEGER | FLOAT)] AS xs"
        ),
        list([int(1), float(2.5)])
    );
}

#[test]
fn value_type_names_the_type_of_a_value() {
    let db = db();
    assert_eq!(
        row(
            &db,
            "RETURN valueType(1), valueType(null), valueType('a'), valueType([1, 'a']), \
             valueType([1, null]), valueType([]), valueType([null]), valueType([[1]]), valueType(date())"
        ),
        [
            "INTEGER NOT NULL",
            "NULL",
            "STRING NOT NULL",
            "LIST<STRING NOT NULL | INTEGER NOT NULL> NOT NULL",
            "LIST<INTEGER> NOT NULL",
            "LIST<NOTHING> NOT NULL",
            "LIST<NULL> NOT NULL",
            "LIST<LIST<INTEGER NOT NULL> NOT NULL> NOT NULL",
            "DATE NOT NULL",
        ]
        .map(string)
    );
    assert_eq!(
        single(
            &db,
            "UNWIND [1, 'a', [2], null, {k: 1}] AS v RETURN all(x IN collect(v) WHERE \
             CASE valueType(x) WHEN 'NULL' THEN x IS NULL ELSE x IS NOT NULL END) AS ok"
        ),
        boolean(true)
    );
}

#[test]
fn cast_converts_or_fails() {
    let db = db();
    assert_eq!(
        row(
            &db,
            "RETURN CAST(1 AS STRING), CAST('12' AS INTEGER), CAST(' 1.5 ' AS FLOAT), CAST('true' AS BOOLEAN), \
             CAST(1.9 AS INTEGER), CAST(null AS INTEGER), CAST(['1', '2'] AS LIST<INTEGER>), CAST(2 AS FLOAT), \
             CAST('x' AS STRING)"
        ),
        [
            string("1"),
            int(12),
            float(1.5),
            boolean(true),
            int(1),
            Value::Null,
            list([int(1), int(2)]),
            float(2.0),
            string("x"),
        ]
    );
    assert_eq!(
        single(
            &db,
            "RETURN CAST('2020-01-02' AS DATE) = date('2020-01-02') AND CAST('P1D' AS DURATION) = duration('P1D') \
             AND CAST(datetime('2020-01-02T03:04:05Z') AS DATE) = date('2020-01-02') AS same"
        ),
        boolean(true)
    );
    for query in [
        "RETURN CAST('abc' AS INTEGER) AS x",
        "RETURN CAST(true AS INTEGER) AS x",
        "RETURN CAST({a: 1} AS STRING) AS x",
        "RETURN CAST([1] AS INTEGER) AS x",
        "RETURN CAST(['a'] AS LIST<INTEGER>) AS x",
    ] {
        assert_eq!(error_kind(&db, query), ErrorKind::InvalidCast, "{query}");
    }
    assert_eq!(
        error_kind(&db, "RETURN CAST('2020-13-01' AS DATE) AS x"),
        ErrorKind::InvalidTemporalValue
    );
    assert_eq!(
        single(&db, "UNWIND ['1', 'x'] AS s RETURN count(*) AS c"),
        int(2)
    );
}

#[test]
fn truth_value_tests_never_give_null() {
    let db = db();
    assert_eq!(
        row(
            &db,
            "RETURN true IS TRUE, false IS TRUE, null IS TRUE, null IS UNKNOWN, null IS NOT TRUE, \
             false IS FALSE, true IS NOT UNKNOWN, (1 < null) IS UNKNOWN"
        ),
        booleans(&[true, false, false, true, true, true, true, true])
    );
    assert_eq!(
        error_kind(&db, "RETURN 1 IS TRUE AS x"),
        ErrorKind::InvalidArgumentType
    );
    assert_eq!(
        error_kind(&db, "UNWIND [1] AS x RETURN x IS TRUE AS y"),
        ErrorKind::TypeMismatch
    );
}

#[test]
fn is_distinct_from_treats_nulls_as_the_same() {
    let db = db();
    assert_eq!(
        row(
            &db,
            "RETURN 1 IS DISTINCT FROM 2, null IS DISTINCT FROM null, null IS NOT DISTINCT FROM 1, \
             [1, null] IS NOT DISTINCT FROM [1, null], 1 IS DISTINCT FROM 1.0, 1 + 1 IS NOT DISTINCT FROM 2"
        ),
        booleans(&[true, false, false, true, false, true])
    );
}

#[test]
fn normalize_and_is_normalized_take_every_normal_form() {
    let db = db();
    assert_eq!(
        row(
            &db,
            "RETURN normalize('A\u{030A}') = '\u{00C5}', size(normalize('\u{00C5}', NFD)), \
             normalize('\u{FB01}', NFKC), normalize('\u{FB01}', 'nfkd'), normalize(null)"
        ),
        [
            boolean(true),
            int(2),
            string("fi"),
            string("fi"),
            Value::Null
        ]
    );
    assert_eq!(
        row(
            &db,
            "RETURN '\u{00C5}' IS NORMALIZED, '\u{00C5}' IS NFD NORMALIZED, 'A\u{030A}' IS NFD NORMALIZED, \
             '\u{FB01}' IS NOT NFKC NORMALIZED, null IS NORMALIZED"
        ),
        [
            boolean(true),
            boolean(false),
            boolean(true),
            boolean(true),
            Value::Null
        ]
    );
    assert_eq!(
        error_kind(&db, "RETURN normalize('a', 'NFX') AS x"),
        ErrorKind::InvalidArgumentValue
    );
    assert_eq!(
        error_kind(&db, "RETURN 1 IS NORMALIZED AS x"),
        ErrorKind::InvalidArgumentType
    );
}

#[test]
fn the_new_words_are_still_names() {
    let db = db();
    assert_eq!(
        row(
            &db,
            "WITH 1 AS typed, 2 AS cast, 3 AS normalized, 4 AS unknown RETURN typed, cast, normalized, unknown"
        ),
        [int(1), int(2), int(3), int(4)]
    );
}
