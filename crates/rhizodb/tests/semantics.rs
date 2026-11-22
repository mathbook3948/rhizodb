mod support;

use rhizodb::ErrorKind;
use rhizodb::Value;
use support::*;

#[test]
fn boolean_operators_use_three_valued_logic() {
    let db = db();
    assert_eq!(
        run(
            &db,
            "RETURN null AND false, null AND true, null OR true, null OR false, \
             null XOR true, NOT null"
        ),
        [vec![
            boolean(false),
            Value::Null,
            boolean(true),
            Value::Null,
            Value::Null,
            Value::Null
        ]]
    );
}

#[test]
fn equality_with_null_is_null_but_grouping_treats_nulls_as_one() {
    let db = db();
    assert_eq!(single(&db, "RETURN null = null"), Value::Null);
    assert_eq!(single(&db, "RETURN [1, null] = [1, null]"), Value::Null);
    assert_eq!(single(&db, "RETURN [1, null] = [2, null]"), boolean(false));
    assert_eq!(
        run(
            &db,
            "UNWIND [null, null, 1] AS x RETURN x, count(*) ORDER BY x"
        ),
        [vec![int(1), int(1)], vec![Value::Null, int(2)]]
    );
}

#[test]
fn null_and_list_predicates_bind_tighter_than_comparisons() {
    let db = db();
    assert_eq!(
        single(&db, "RETURN false IS NULL = null IS NULL"),
        boolean(false)
    );
    assert_eq!(single(&db, "RETURN 1 IN [1] = true"), boolean(true));
}

#[test]
fn in_returns_null_when_only_a_null_could_match() {
    let db = db();
    assert_eq!(single(&db, "RETURN 1 IN [1, null]"), boolean(true));
    assert_eq!(single(&db, "RETURN 2 IN [1, null]"), Value::Null);
    assert_eq!(single(&db, "RETURN 2 IN [1, 3]"), boolean(false));
    assert_eq!(single(&db, "RETURN null IN []"), boolean(false));
}

#[test]
fn integers_and_floats_compare_numerically() {
    let db = db();
    assert_eq!(
        run(&db, "RETURN 1 = 1.0, 1 < 1.5, 2 > 1.5, 1 <= 1.0"),
        [vec![boolean(true); 4]]
    );
    assert_eq!(single(&db, "RETURN 1 < 'a'"), Value::Null);
    assert_eq!(single(&db, "RETURN 0.0 / 0.0 = 0.0 / 0.0"), boolean(false));
}

#[test]
fn order_by_uses_the_global_order_across_types() {
    let db = db();
    let rows = run(
        &db,
        "UNWIND [null, 1, 'a', true, [1], {k: 1}, 2.5] AS x RETURN x ORDER BY x",
    );
    assert_eq!(
        rows,
        [
            vec![map(&[("k", int(1))])],
            vec![list([int(1)])],
            vec![string("a")],
            vec![boolean(true)],
            vec![int(1)],
            vec![float(2.5)],
            vec![Value::Null],
        ]
    );
    let rows = run(&db, "UNWIND [1, null, 2] AS x RETURN x ORDER BY x DESC");
    assert_eq!(rows, [vec![Value::Null], vec![int(2)], vec![int(1)]]);
}

#[test]
fn integer_arithmetic_stays_integer_and_reports_overflow() {
    let db = db();
    assert_eq!(
        run(&db, "RETURN 7 / 2, 7 % 3, -7 / 2, 2 ^ 3, 1 + 1.5"),
        [vec![int(3), int(1), int(-3), float(8.0), float(2.5)]]
    );
    assert_eq!(
        error_kind(&db, "RETURN 9223372036854775807 + 1"),
        ErrorKind::IntegerOverflow
    );
    assert_eq!(
        error_kind(&db, "RETURN -9223372036854775808 / -1"),
        ErrorKind::IntegerOverflow
    );
    assert_eq!(error_kind(&db, "RETURN 1 / 0"), ErrorKind::DivisionByZero);
    assert_eq!(error_kind(&db, "RETURN 1 % 0"), ErrorKind::DivisionByZero);
    assert_eq!(single(&db, "RETURN 1.0 / 0"), float(f64::INFINITY));
}

#[test]
fn text_converts_to_a_number_only_when_it_is_a_decimal_number() {
    let db = db();
    for text in [
        "inf",
        "-inf",
        "+inf",
        "Infinity",
        "-Infinity",
        "nan",
        "NaN",
        "1e999",
        "-1e999",
        "0x10",
        "1_000",
        "1e",
        "--1",
        "",
    ] {
        assert_eq!(
            run(
                &db,
                &format!("RETURN toFloat('{text}') AS f, toInteger('{text}') AS i")
            ),
            [vec![Value::Null, Value::Null]],
            "{text:?}"
        );
    }
    for (text, float_value, integer_value) in [
        ("1.5", 1.5, 1),
        ("-2", -2.0, -2),
        ("+3", 3.0, 3),
        ("1e3", 1000.0, 1000),
        (".5", 0.5, 0),
        ("2.5E-1", 0.25, 0),
        (" 7 ", 7.0, 7),
    ] {
        assert_eq!(
            run(
                &db,
                &format!("RETURN toFloat('{text}') AS f, toInteger('{text}') AS i")
            ),
            [vec![float(float_value), int(integer_value)]],
            "{text:?}"
        );
    }
}

#[test]
fn the_remainder_by_minus_one_is_zero_even_for_the_smallest_integer() {
    let db = db();
    assert_eq!(
        run(
            &db,
            "RETURN -9223372036854775808 % -1 AS m, 9223372036854775807 % -1 AS n, -7 % -1 AS k"
        ),
        [vec![int(0), int(0), int(0)]]
    );
}

#[test]
fn plus_concatenates_strings_and_lists() {
    let db = db();
    assert_eq!(
        run(&db, "RETURN 'a' + 'b', [1] + [2], [1] + 2, 0 + [1]"),
        [vec![
            string("ab"),
            list([int(1), int(2)]),
            list([int(1), int(2)]),
            list([int(0), int(1)])
        ]]
    );
}

#[test]
fn implicit_grouping_keys_are_the_non_aggregated_items() {
    let db = db();
    let rows = run(
        &db,
        "UNWIND [1, 2, 3, 4] AS x RETURN x % 2 AS parity, sum(x) AS total ORDER BY parity",
    );
    assert_eq!(rows, [vec![int(0), int(6)], vec![int(1), int(4)]]);
}

#[test]
fn next_to_an_aggregate_only_simple_keys_and_projected_names_are_visible() {
    let db = db();
    let rows = run(
        &db,
        "UNWIND [{a: 1}, {a: 1}, {a: 1}, {a: 5}] AS m \
         RETURN m.a AS a, m.a + count(*) AS score ORDER BY m.a + count(*) DESC",
    );
    assert_eq!(rows, [vec![int(5), int(6)], vec![int(1), int(4)]]);
    for (query, kind) in [
        (
            "UNWIND [1, 2] AS x RETURN x + 1, x + 1 + count(*)",
            ErrorKind::AmbiguousAggregationExpression,
        ),
        (
            "UNWIND [1, 2] AS x WITH count(*) AS c ORDER BY x + count(*) RETURN c",
            ErrorKind::UndefinedVariable,
        ),
        (
            "UNWIND [1, 2] AS x WITH count(*) AS c ORDER BY sum(x) RETURN c",
            ErrorKind::UndefinedVariable,
        ),
        (
            "RETURN [x IN [1, 2] | count(*)]",
            ErrorKind::InvalidAggregation,
        ),
        (
            "UNWIND [1, 2] AS x WITH x + 1, count(*) AS c ORDER BY x + 1 + count(*) RETURN c",
            ErrorKind::AmbiguousAggregationExpression,
        ),
        (
            "UNWIND [1, 2] AS x WITH x + 1, count(*) AS c RETURN c",
            ErrorKind::NoExpressionAlias,
        ),
        (
            "UNWIND [1] AS a WITH a + 1, a + 1 RETURN 1",
            ErrorKind::NoExpressionAlias,
        ),
        (
            "UNWIND [1] AS a RETURN a + 1, a + 1",
            ErrorKind::ColumnNameConflict,
        ),
    ] {
        assert_eq!(error_kind(&db, query), kind, "{query}");
    }
}

#[test]
fn an_aggregate_argument_cannot_call_rand() {
    let db = db();
    for query in [
        "RETURN count(rand())",
        "MATCH (n:Missing) RETURN collect(n.x + rand())",
    ] {
        assert_eq!(
            error_kind(&db, query),
            ErrorKind::NonConstantExpression,
            "{query}"
        );
    }
    assert_eq!(ints(&run(&db, "RETURN count(*) + toInteger(rand())")), [1]);
}

#[test]
fn skip_and_limit_must_be_non_negative_integer_constants() {
    let db = db();
    assert_eq!(
        error_kind(&db, "UNWIND [1] AS x RETURN x LIMIT -1"),
        ErrorKind::NegativeIntegerArgument
    );
    assert_eq!(
        error_kind(&db, "UNWIND [1] AS x RETURN x SKIP 1.5"),
        ErrorKind::InvalidArgumentType
    );
    assert_eq!(
        error_kind(&db, "UNWIND [1] AS x RETURN x LIMIT x"),
        ErrorKind::NonConstantExpression
    );
}

#[test]
fn type_errors_in_the_query_text_fail_even_without_rows() {
    let db = db();
    for query in [
        "MATCH (n) RETURN 1 AND 'a'",
        "MATCH (n) RETURN NOT 3",
        "MATCH (n) RETURN 1 IN true",
        "MATCH (n) RETURN any(x IN ['a'] WHERE x % 2 = 0)",
    ] {
        assert_eq!(
            error_kind(&db, query),
            ErrorKind::InvalidArgumentType,
            "{query}"
        );
    }
}

#[test]
fn statically_known_kinds_are_checked_before_running() {
    let db = db();
    for query in [
        "MATCH p = (a) RETURN labels(p)",
        "MATCH (n) RETURN type(n)",
        "MATCH (r)-[]->() RETURN length(r)",
        "MATCH p = ()-[*]->() RETURN size(p)",
        "MATCH p = ()-->() WHERE p.name = 'x' RETURN p",
        "MATCH (n) WHERE n RETURN n",
        "RETURN properties(1)",
        "MATCH () DELETE 1 + 1",
    ] {
        assert_eq!(
            error_kind(&db, query),
            ErrorKind::InvalidArgumentType,
            "{query}"
        );
    }
    assert_eq!(
        error_kind(&db, "MATCH (n) DELETE n:L"),
        ErrorKind::InvalidDelete
    );
}

#[test]
fn type_errors_found_in_data_are_type_mismatches() {
    let db = db();
    assert_eq!(
        error_kind(&db, "UNWIND [true, 'a'] AS x RETURN x AND true"),
        ErrorKind::TypeMismatch
    );
    assert_eq!(
        error_kind(&db, "WITH 123 AS list RETURN list[0]"),
        ErrorKind::TypeMismatch
    );
    assert_eq!(
        run(&db, "UNWIND [] AS x RETURN x AND true"),
        Vec::<Vec<Value>>::new()
    );
}

#[test]
fn binding_errors_are_reported_before_running() {
    let db = db();
    assert_eq!(
        error_kind(&db, "RETURN missing"),
        ErrorKind::UndefinedVariable
    );
    assert_eq!(
        error_kind(&db, "MATCH (a) WITH a MATCH (a)-[a]->() RETURN a"),
        ErrorKind::VariableTypeConflict
    );
    assert_eq!(
        error_kind(&db, "MATCH (r)-[r]->() RETURN r"),
        ErrorKind::VariableTypeConflict
    );
    assert_eq!(
        error_kind(&db, "CREATE (a)-[:R]->(a:L) RETURN a"),
        ErrorKind::VariableAlreadyBound
    );
    assert_eq!(
        error_kind(&db, "UNWIND [1] AS x UNWIND [2] AS x RETURN x"),
        ErrorKind::VariableAlreadyBound
    );
    for query in [
        "MATCH p = (p)-[]->() RETURN p",
        "MATCH (a) CREATE (a)",
        "MATCH (a) MERGE (a)",
        "MATCH ()-[r]->() CREATE ()-[r]->()",
        "MATCH (a)-[r]->(b) MERGE (a)-[r]->(b)",
    ] {
        assert_eq!(
            error_kind(&db, query),
            ErrorKind::VariableAlreadyBound,
            "{query}"
        );
    }
    assert_eq!(
        error_kind(&db, "MATCH (n) WHERE (n)-[r]->() RETURN n"),
        ErrorKind::UndefinedVariable
    );
    assert_eq!(
        run(&db, "MATCH (n) WHERE EXISTS { (n)-[r]->() } RETURN n"),
        Vec::<Vec<Value>>::new()
    );
    assert_eq!(
        error_kind(&db, "MATCH (a)-[r]->()-[r]->() RETURN r"),
        ErrorKind::RelationshipUniquenessViolation
    );
    assert_eq!(
        error_kind(&db, "MATCH (n) RETURN n.x, count(*) + n.y"),
        ErrorKind::AmbiguousAggregationExpression
    );
    assert_eq!(
        error_kind(&db, "MATCH (n) WHERE count(n) > 1 RETURN n"),
        ErrorKind::InvalidAggregation
    );
    assert_eq!(
        error_kind(&db, "RETURN nope(1)"),
        ErrorKind::UnknownFunction
    );
    assert_eq!(
        error_kind(&db, "CREATE ()-[:A|B]->()"),
        ErrorKind::NoSingleRelationshipType
    );
    assert_eq!(
        error_kind(&db, "CREATE ()-[:A]-()"),
        ErrorKind::RequiresDirectedRelationship
    );
    assert_eq!(
        error_kind(&db, "RETURN $absent"),
        ErrorKind::MissingParameter
    );
}

#[test]
fn syntax_errors_carry_the_parser_kind() {
    let db = db();
    assert!(matches!(
        error_kind(&db, "RETURN 1 +"),
        ErrorKind::Syntax(rhizodb::SyntaxErrorKind::UnexpectedEnd)
    ));
}

#[test]
fn properties_reject_values_that_cannot_be_stored() {
    let db = db();
    run(&db, "CREATE (:N)");
    assert_eq!(
        error_kind(&db, "MATCH (n:N) SET n.x = {a: 1}"),
        ErrorKind::InvalidPropertyType
    );
    assert_eq!(
        error_kind(&db, "MATCH (n:N) SET n.x = [n]"),
        ErrorKind::InvalidPropertyType
    );
    let nested = |depth: usize| (0..depth).fold(int(1), |inner, _| list([inner]));
    let set_nested = |depth: usize| {
        let mut parameters = rhizodb::Parameters::new();
        parameters.insert("deep".to_owned(), nested(depth));
        db.execute("MATCH (n:N) SET n.deep = $deep", &parameters)
    };
    assert_eq!(
        set_nested(65).unwrap_err().kind(),
        ErrorKind::InvalidPropertyType
    );
    set_nested(64).unwrap();
    assert_eq!(single(&db, "MATCH (n:N) RETURN n.deep"), nested(64));
    run(&db, "MATCH (n:N) SET n.x = [1, 'two', 3.0]");
    assert_eq!(
        single(&db, "MATCH (n:N) RETURN n.x"),
        list([int(1), string("two"), float(3.0)])
    );
}

#[test]
fn reading_a_deleted_element_fails_except_for_the_type_of_a_relationship() {
    let db = db();
    run(&db, "CREATE (:L {x: 1})-[:T {y: 2}]->()");
    for query in [
        "MATCH (n:L) DETACH DELETE n RETURN n.x",
        "MATCH (n:L) DETACH DELETE n RETURN labels(n)",
        "MATCH ()-[r]->() DELETE r RETURN r.y",
    ] {
        assert_eq!(
            error_kind(&db, query),
            ErrorKind::DeletedEntityAccess,
            "{query}"
        );
    }
    assert_eq!(
        run(&db, "MATCH ()-[r]->() DELETE r RETURN type(r), r:T"),
        [vec![string("T"), boolean(true)]]
    );
}

#[test]
fn the_type_of_a_deleted_relationship_stays_readable_in_every_clause_after_the_delete() {
    for query in [
        "MATCH ()-[r]->() DELETE r RETURN 1 AS one ORDER BY type(r)",
        "MATCH ()-[r]->() DELETE r WITH r ORDER BY type(r) RETURN 1 AS one",
        "MATCH ()-[r]->() DELETE r WITH r WHERE type(r) = 'T' RETURN 1 AS one",
        "MATCH ()-[r]->() DELETE r WITH type(r) AS t RETURN 1 AS one",
        "MATCH ()-[r]->() DELETE r FILTER r:T RETURN 1 AS one",
        "MATCH ()-[r]->() DELETE r LET t = type(r) RETURN 1 AS one",
    ] {
        let db = db();
        run(&db, "CREATE ()-[:T]->()");
        assert_eq!(run(&db, query), [vec![int(1)]], "{query}");
    }
    for query in [
        "MATCH (a)-[r]->() DELETE r SET a.t = type(r)",
        "MATCH ()-[r]->() DELETE r WITH r CREATE ({t: type(r)})",
        "MATCH ()-[r]->() DELETE r WITH r MERGE ({t: type(r)})",
    ] {
        let db = db();
        run(&db, "CREATE ()-[:T]->()");
        run(&db, query);
        assert_eq!(
            single(&db, "MATCH (n) WHERE n.t = 'T' RETURN count(*)"),
            int(1),
            "{query}"
        );
    }
}

#[test]
fn a_label_test_on_a_relationship_tests_its_type() {
    let db = db();
    run(&db, "CREATE ()-[:T]->(), ()-[:U]->()");
    assert_eq!(
        run(
            &db,
            "MATCH ()-[r]->() RETURN type(r), r:T, r:T:T, r:T:U ORDER BY type(r)"
        ),
        [
            vec![string("T"), boolean(true), boolean(true), boolean(false)],
            vec![string("U"), boolean(false), boolean(false), boolean(false)],
        ]
    );
    assert_eq!(
        error_kind(&db, "MATCH p = ()-->() RETURN p:T"),
        ErrorKind::InvalidArgumentType
    );
}

#[test]
fn an_expression_conditional_evaluation_skips_never_fails_on_values() {
    let db = db();
    assert!(
        run(
            &db,
            "MATCH (n) WHERE false RETURN CASE WHEN n IS NULL THEN 0 ELSE 1 / 0 END"
        )
        .is_empty()
    );
    run(&db, "CREATE (:N)");
    assert_eq!(
        single(
            &db,
            "MATCH (n:N) RETURN CASE WHEN n IS NOT NULL THEN 0 ELSE 1 / 0 END"
        ),
        int(0)
    );
    assert_eq!(
        single(&db, "RETURN CASE WHEN true THEN 1 ELSE 1 / 0 END"),
        int(1)
    );
    assert_eq!(
        single(&db, "RETURN [x IN [] | 1 / 0]"),
        Value::List(Vec::new())
    );
    assert_eq!(
        error_kind(
            &db,
            "MATCH (n:N) RETURN CASE WHEN n IS NULL THEN 0 ELSE 1 / 0 END"
        ),
        ErrorKind::DivisionByZero
    );
}

#[test]
fn parameter_values_are_data_not_query_text() {
    let db = db();
    let run_with = |query: &str, flag: Option<bool>| {
        let mut parameters = rhizodb::Parameters::new();
        parameters.insert("a".to_owned(), string("x"));
        if let Some(flag) = flag {
            parameters.insert("flag".to_owned(), boolean(flag));
        }
        db.execute(query, &parameters)
    };
    let branch = "RETURN CASE WHEN $flag THEN $a - 1 ELSE $a END";
    assert_eq!(
        run_with(branch, Some(false)).unwrap().rows(),
        [vec![string("x")]]
    );
    assert_eq!(
        run_with(branch, Some(true)).unwrap_err().kind(),
        ErrorKind::TypeMismatch
    );
    assert!(
        run_with("MATCH (n) RETURN $a - 1", None)
            .unwrap()
            .rows()
            .is_empty()
    );
}

#[test]
fn range_refuses_a_list_too_long_to_hold() {
    let db = db();
    assert_eq!(
        single(&db, "RETURN size(range(1, 1000001))"),
        int(1_000_001)
    );
    for query in [
        "RETURN size(range(1, 10000001))",
        "RETURN size(range(0, 10000000000))",
        "RETURN size(range(10000000000, 0, -1))",
        "RETURN size(range(-9223372036854775807 - 1, 9223372036854775807))",
    ] {
        assert_eq!(
            error_kind(&db, query),
            ErrorKind::NumberOutOfRange,
            "{query}"
        );
    }
    assert_eq!(single(&db, "RETURN size(range(5, 1))"), int(0));
    assert_eq!(
        single(&db, "RETURN size(range(0, 10000000000, 10000000000))"),
        int(2)
    );
}

#[test]
fn a_percentile_must_be_the_same_for_every_row_of_a_group() {
    let db = db();
    assert_eq!(
        error_kind(
            &db,
            "UNWIND [[10, 0.0], [20, 1.0]] AS r RETURN percentileDisc(r[0], r[1])"
        ),
        ErrorKind::InvalidArgumentValue
    );
    assert_eq!(
        error_kind(
            &db,
            "UNWIND [[10, 0.0], [20, 1.0]] AS r RETURN percentileCont(r[0], r[1])"
        ),
        ErrorKind::InvalidArgumentValue
    );
    assert_eq!(
        run(
            &db,
            "UNWIND [[10, 0.0, 'a'], [20, 1.0, 'b'], [30, 1.0, 'b']] AS r \
             RETURN r[2] AS g, percentileDisc(r[0], r[1]) AS p ORDER BY g"
        ),
        [vec![string("a"), int(10)], vec![string("b"), int(30)]]
    );
    assert_eq!(
        single(
            &db,
            "UNWIND [10, 20, 30] AS x RETURN percentileCont(x, 0.5)"
        ),
        float(20.0)
    );
    assert_eq!(
        single(
            &db,
            "UNWIND [10, 20] AS x RETURN percentileDisc(x, CASE x WHEN 10 THEN 1 ELSE 1.0 END)"
        ),
        int(20)
    );
}

#[test]
fn a_percentile_over_no_rows_is_null_without_reading_the_percentile() {
    let db = db();
    assert_eq!(
        single(
            &db,
            "UNWIND [] AS x RETURN percentileDisc(x, [p IN [0.5] | p][0]) AS c"
        ),
        Value::Null
    );
    assert_eq!(
        single(&db, "UNWIND [] AS x RETURN percentileCont(x, 2.0) AS c"),
        Value::Null
    );
    run(&db, "CREATE (:A)");
    assert_eq!(
        single(
            &db,
            "MATCH (a:A) WITH 0.5 AS p OPTIONAL MATCH (m:Missing) \
             WITH p, m WHERE m IS NOT NULL RETURN percentileCont(m.x, p)"
        ),
        Value::Null
    );
    assert_eq!(
        single(
            &db,
            "UNWIND [1, 2] AS x RETURN percentileCont(x, null) AS c"
        ),
        Value::Null
    );
}

#[test]
fn floats_print_like_the_reference_implementation() {
    let db = db();
    for (value, text) in [
        ("1e20", "1.0E20"),
        ("-1e20", "-1.0E20"),
        ("1.5e-7", "1.5E-7"),
        ("12345678.9", "1.23456789E7"),
        ("1.0e7", "1.0E7"),
        ("0.0001", "1.0E-4"),
        ("9999999.0", "9999999.0"),
        ("0.001", "0.001"),
        ("100.0", "100.0"),
        ("2.3", "2.3"),
        ("0.0", "0.0"),
    ] {
        assert_eq!(
            single(&db, &format!("RETURN toString({value})")),
            string(text),
            "{value}"
        );
    }
    assert_eq!(single(&db, "RETURN 'x' + 1e20"), string("x1.0E20"));
}

#[test]
fn a_quantifier_variable_that_reuses_a_projected_name_is_its_own_variable() {
    let db = db();
    assert_eq!(
        ints(&run(
            &db,
            "UNWIND [1, 2] AS x WITH DISTINCT x AS y WHERE any(x IN [5] WHERE x > y) RETURN y \
             ORDER BY y"
        )),
        [1, 2]
    );
}

#[test]
fn a_rebound_name_in_with_distinct_where_is_the_new_binding() {
    let db = db();
    run(&db, "CREATE ({x: 1}), ({x: 2})");
    assert_eq!(
        ints(&run(
            &db,
            "MATCH (n {x: 1}), (m {x: 2}) WITH DISTINCT n.x AS nx, m AS n WHERE n.x = 2 \
             RETURN nx"
        )),
        [1]
    );
}

#[test]
fn a_comprehension_variable_that_reuses_a_grouping_key_name_is_its_own_variable() {
    let db = db();
    assert_eq!(
        run(
            &db,
            "UNWIND [1, 2] AS x RETURN x, count(*) + [x IN [9] | x][0] AS c ORDER BY x"
        ),
        [vec![int(1), int(10)], vec![int(2), int(10)]]
    );
}

#[test]
fn order_by_and_where_after_with_distinct_read_projected_expressions_alike() {
    let db = db();
    run(
        &db,
        "CREATE ({name: 'a'}), ({name: 'b'}), ({name: 'c'}), ({name: 'c'})",
    );
    assert_eq!(
        run(
            &db,
            "MATCH (n) WITH DISTINCT n.name AS name ORDER BY toUpper(n.name) DESC \
             WHERE n.name <> 'a' RETURN name"
        ),
        [vec![string("c")], vec![string("b")]]
    );
}

const CHAIN: &str = "CREATE (:N {i: 1})-[:R]->(:N {i: 2})-[:R]->(:N {i: 3})";

#[test]
fn a_pattern_next_to_an_aggregate_sees_the_grouped_variable() {
    let db = db();
    run(&db, CHAIN);
    assert_eq!(
        run(
            &db,
            "MATCH (a:N) WITH a, count(*) + size([(a)-->(x) | x]) AS v RETURN a.i, v ORDER BY a.i"
        ),
        [
            vec![int(1), int(2)],
            vec![int(2), int(2)],
            vec![int(3), int(1)]
        ]
    );
    assert_eq!(
        run(
            &db,
            "MATCH (a:N) WITH a, count(*) AS c, count(*) > 0 AND exists { (a)-->() } AS e RETURN a.i, c, e ORDER BY a.i"
        ),
        [
            vec![int(1), int(1), boolean(true)],
            vec![int(2), int(1), boolean(true)],
            vec![int(3), int(1), boolean(false)]
        ]
    );
}

#[test]
fn a_pattern_next_to_an_aggregate_cannot_use_a_variable_that_is_not_a_grouping_key() {
    let db = db();
    run(&db, CHAIN);
    assert_eq!(
        error_kind(
            &db,
            "MATCH (a:N) RETURN a.i AS i, count(*) + size([(a)-->(x) | x]) AS v"
        ),
        ErrorKind::AmbiguousAggregationExpression
    );
    assert_eq!(
        error_kind(
            &db,
            "MATCH (a:N) RETURN a.i AS i, count(*) AS c ORDER BY exists { (a)-->() }"
        ),
        ErrorKind::UndefinedVariable
    );
}

#[test]
fn a_pattern_after_with_distinct_sees_only_projected_variables() {
    let db = db();
    run(&db, CHAIN);
    assert_eq!(
        error_kind(
            &db,
            "MATCH (n:N) WITH DISTINCT n.i AS i WHERE size([(n)-->(m) | m]) = 0 RETURN i"
        ),
        ErrorKind::UndefinedVariable
    );
    assert_eq!(
        run(
            &db,
            "MATCH (n:N) WITH DISTINCT n AS n WHERE size([(n)-->(m) | m]) = 0 RETURN n.i"
        ),
        [vec![int(3)]]
    );
}

#[test]
fn a_bad_parameter_is_the_same_data_error_as_the_same_value_in_a_variable() {
    let db = db();
    let kind_with = |query: &str| {
        let mut parameters = rhizodb::Parameters::new();
        parameters.insert("p".to_owned(), int(1));
        db.execute(query, &parameters).unwrap_err().kind()
    };
    for (direct, through_variable) in [
        ("RETURN $p.x", "WITH $p AS q RETURN q.x"),
        ("RETURN size($p)", "WITH $p AS q RETURN size(q)"),
    ] {
        assert_eq!(
            kind_with(direct),
            kind_with(through_variable),
            "{direct} against {through_variable}"
        );
    }
    assert_eq!(
        kind_with("RETURN $p.x"),
        ErrorKind::PropertyAccessOnNonEntity
    );
    assert_eq!(
        kind_with("RETURN size($p)"),
        ErrorKind::InvalidArgumentValue
    );
}

#[test]
fn skip_and_limit_may_use_a_comprehension_variable_but_no_outer_variable() {
    let db = db();
    assert_eq!(
        run(
            &db,
            "UNWIND [1, 2, 3] AS i RETURN i ORDER BY i LIMIT size([x IN [1, 2] | x])"
        ),
        [vec![int(1)], vec![int(2)]]
    );
    assert_eq!(
        error_kind(
            &db,
            "UNWIND [1, 2, 3] AS i RETURN i LIMIT size([x IN [1, 2] | i])"
        ),
        ErrorKind::NonConstantExpression
    );
}

#[test]
fn replace_names_the_argument_that_is_wrong() {
    let db = db();
    for (literal, query, wrong) in [
        (
            "RETURN replace('abc', 'b', 1)",
            "UNWIND [1] AS x RETURN replace('abc', 'b', x)",
            "integer",
        ),
        (
            "RETURN replace('abc', true, 'x')",
            "UNWIND [true] AS x RETURN replace('abc', x, 'x')",
            "boolean",
        ),
        (
            "RETURN replace('abc', 2, 1.5)",
            "UNWIND [[2, 1.5]] AS x RETURN replace('abc', x[0], x[1])",
            "integer",
        ),
    ] {
        assert_eq!(
            error_kind(&db, literal),
            ErrorKind::InvalidArgumentType,
            "{literal}"
        );
        let error = db.execute(query, &rhizodb::Parameters::new()).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidArgumentValue, "{query}");
        assert!(
            error.to_string().contains(wrong) && !error.to_string().contains("string"),
            "{query}: {error}"
        );
    }
    assert_eq!(single(&db, "RETURN replace('abc', 'b', null)"), Value::Null);
    assert_eq!(
        single(&db, "RETURN replace('abc', 'b', 'x')"),
        string("axc")
    );
}

fn fails_before_reading_data(db: &rhizodb::Database, query: &str) {
    let error = db
        .execute(query, &rhizodb::Parameters::new())
        .expect_err(query);
    assert!(
        matches!(
            error.kind(),
            ErrorKind::InvalidArgumentType | ErrorKind::PropertyAccessOnNonEntity
        ),
        "{query}: {error} ({:?})",
        error.kind()
    );
}

#[test]
fn a_type_error_in_the_query_text_fails_before_any_row_reaches_it() {
    let db = db();
    for query in [
        "RETURN CASE WHEN false THEN (1).x ELSE 2 END",
        "RETURN CASE WHEN false THEN true + 1 ELSE 2 END",
        "RETURN false AND (1).x",
        "RETURN [x IN [] | size(1)]",
        "RETURN [x IN [] WHERE 1]",
        "RETURN all(x IN [] WHERE 1)",
        "MATCH (n:Missing) RETURN CASE WHEN n IS NULL THEN 1 ELSE true + 1 END",
    ] {
        fails_before_reading_data(&db, query);
    }
}

#[test]
fn an_error_in_an_operand_is_reported_before_the_type_check_on_it() {
    let db = db();
    for (query, kind) in [
        ("RETURN [x][true]", ErrorKind::UndefinedVariable),
        ("RETURN 1 + true + x", ErrorKind::UndefinedVariable),
        ("RETURN -(x + true)", ErrorKind::UndefinedVariable),
        ("RETURN (x + true)[0..1]", ErrorKind::UndefinedVariable),
        ("RETURN (x + true):A", ErrorKind::UndefinedVariable),
        ("RETURN (x + true).k", ErrorKind::UndefinedVariable),
        ("RETURN size(x + true)", ErrorKind::UndefinedVariable),
        (
            "RETURN CASE WHEN x + true THEN 1 END",
            ErrorKind::UndefinedVariable,
        ),
        (
            "MATCH (n) WHERE x + true RETURN n",
            ErrorKind::UndefinedVariable,
        ),
        (
            "UNWIND [1] AS y RETURN sum(x + true)",
            ErrorKind::UndefinedVariable,
        ),
        ("RETURN 1 SKIP [x][true]", ErrorKind::NonConstantExpression),
    ] {
        assert_eq!(error_kind(&db, query), kind, "{query}");
    }
}

#[test]
fn a_reading_clause_cannot_follow_an_updating_clause_without_with() {
    let db = db();
    run(&db, "CREATE (:Q)");
    for query in [
        "CREATE (a:Q) MATCH (b:Q) RETURN count(b) AS c",
        "CREATE (a:Q) OPTIONAL MATCH (b:Q) RETURN count(b) AS c",
        "CREATE (a:Q) UNWIND [1] AS x RETURN x",
        "MERGE (a:Q) MATCH (b:Q) RETURN count(b) AS c",
        "MATCH (a:Q) SET a.v = 1 MATCH (b:Q) RETURN count(b) AS c",
        "MATCH (a:Q) REMOVE a.v MATCH (b:Q) RETURN count(b) AS c",
        "MATCH (a:Q) DELETE a MATCH (b:Q) RETURN count(b) AS c",
        "MATCH (a:Q) SET a.v = 1 RETURN a UNION CREATE (c:Q) MATCH (d:Q) RETURN d",
    ] {
        assert_eq!(
            error_kind(&db, query),
            ErrorKind::InvalidClauseComposition,
            "{query}"
        );
    }
    assert_eq!(
        single(&db, "CREATE (a:Q) WITH a MATCH (b:Q) RETURN count(b) AS c"),
        int(2)
    );
    assert_eq!(
        single(
            &db,
            "CREATE (a:Q) SET a.v = 1 WITH a UNWIND [1, 2] AS x RETURN count(x) AS c"
        ),
        int(2)
    );
    assert_eq!(
        single(
            &db,
            "CREATE (a:Z) CREATE (b:Z) SET b.v = 1 RETURN count(*) AS n"
        ),
        int(1)
    );
}

#[test]
fn a_type_error_fails_before_any_row_reaches_it_in_every_place_an_expression_can_stand() {
    let db = db();
    let mut escaped = Vec::new();
    for (template, wrong, right) in [
        ("MATCH (n) RETURN count({}) AS c", "1 - 'a'", "1 - 1"),
        (
            "MATCH (n) RETURN collect(DISTINCT {}) AS c",
            "1 - 'a'",
            "1 - 1",
        ),
        (
            "MATCH (n) RETURN n.x AS x, max({}) AS c",
            "1 - 'a'",
            "1 - 1",
        ),
        ("MATCH (n) WITH count({}) AS c RETURN c", "1 - 'a'", "1 - 1"),
        (
            "MATCH (n) RETURN count(n.x + ({})) AS c",
            "1 - 'a'",
            "1 - 1",
        ),
        (
            "MATCH (n) RETURN count(*) AS c ORDER BY sum({})",
            "'a'",
            "1",
        ),
        ("MATCH (n) RETURN sum({}) AS s", "'a'", "1"),
        ("MATCH (n) RETURN avg({}) AS s", "'a'", "1"),
        ("MATCH (n) RETURN stDev({}) AS s", "'a'", "1"),
        ("MATCH (n) RETURN stDevP({}) AS s", "'a'", "1"),
        (
            "MATCH (n) RETURN percentileCont(n.x, {}) AS s",
            "'a'",
            "0.5",
        ),
        ("MATCH (n) RETURN percentileDisc({}, 0.5) AS s", "'a'", "1"),
        ("MATCH (n) WHERE ({}) = 1 RETURN n", "1 - 'a'", "1 - 1"),
        (
            "MATCH (n) WHERE (n)-->({x: {}}) RETURN n",
            "1 - 'a'",
            "1 - 1",
        ),
        (
            "MATCH (n) WHERE exists((n)-->({x: {}})) RETURN n",
            "1 - 'a'",
            "1 - 1",
        ),
        (
            "MATCH (n) WHERE EXISTS { (n)-->({x: {}}) } RETURN n",
            "1 - 'a'",
            "1 - 1",
        ),
        (
            "MATCH (n) WHERE EXISTS { (n)-->(m) WHERE {} = 1 } RETURN n",
            "1 - 'a'",
            "1 - 1",
        ),
        (
            "MATCH (n) WHERE EXISTS { MATCH (n)-->(m) WITH m WHERE {} = 1 RETURN m } RETURN n",
            "1 - 'a'",
            "1 - 1",
        ),
        (
            "MATCH (n) RETURN [(n)-->(m) WHERE {} = 1 | m] AS l",
            "1 - 'a'",
            "1 - 1",
        ),
        ("MATCH (n) RETURN [(n)-->(m) | {}] AS l", "1 - 'a'", "1 - 1"),
        (
            "MATCH (n) RETURN [(n)-->({x: {}}) | 1] AS l",
            "1 - 'a'",
            "1 - 1",
        ),
    ] {
        let query = template.replace("{}", wrong);
        if !matches!(
            db.execute(&query, &rhizodb::Parameters::new())
                .map_err(|error| error.kind()),
            Err(ErrorKind::InvalidArgumentType | ErrorKind::PropertyAccessOnNonEntity)
        ) {
            escaped.push(query);
        }
        let query = template.replace("{}", right);
        assert!(
            db.execute(&query, &rhizodb::Parameters::new()).is_ok(),
            "{query}"
        );
    }
    assert!(
        escaped.is_empty(),
        "these did not fail before reading data: {escaped:#?}"
    );
}

#[test]
fn an_error_inside_an_aggregate_that_depends_on_values_waits_for_a_row() {
    let db = db();
    assert_eq!(single(&db, "MATCH (n) RETURN sum(1 / 0) AS s"), int(0));
    assert_eq!(
        single(&db, "MATCH (n) RETURN count(n.x - 'a') AS c"),
        int(0)
    );
    assert_eq!(
        run(&db, "MATCH (n) WHERE exists((n)-->({x: 1 / 0})) RETURN n"),
        Vec::<Vec<Value>>::new()
    );
}

#[test]
fn a_type_error_in_a_merge_action_fails_even_when_the_action_is_not_run() {
    let db = db();
    run(&db, "CREATE (:A)");
    fails_before_reading_data(&db, "MERGE (n:A) ON CREATE SET n.v = 'a' - 1 RETURN n");
    fails_before_reading_data(&db, "MERGE (n:A) ON MATCH SET n.v = 'a' - 1 RETURN n");
    fails_before_reading_data(&db, "MATCH (n:Missing) SET n.v = 'a' - 1");
}

#[test]
fn an_error_that_depends_on_values_fails_only_when_a_row_reaches_it() {
    let db = db();
    assert_eq!(
        run(
            &db,
            "MATCH (n:Missing) RETURN CASE WHEN n IS NULL THEN 1 / 0 ELSE 2 END"
        ),
        Vec::<Vec<Value>>::new()
    );
    assert_eq!(
        single(
            &db,
            "RETURN CASE WHEN false THEN 9223372036854775807 + 1 ELSE 2 END"
        ),
        int(2)
    );
    assert_eq!(
        error_kind(&db, "RETURN CASE WHEN true THEN 1 / 0 ELSE 2 END"),
        ErrorKind::DivisionByZero
    );
}

#[test]
fn a_parameter_is_data_so_its_type_error_waits_for_a_row() {
    let db = db();
    let mut parameters = rhizodb::Parameters::new();
    parameters.insert("flag".to_owned(), boolean(false));
    parameters.insert("a".to_owned(), string("x"));
    let result = db
        .execute(
            "RETURN CASE WHEN $flag THEN $a - 1 ELSE $a END",
            &parameters,
        )
        .unwrap();
    assert_eq!(result.rows(), [vec![string("x")]]);

    let mut parameters = rhizodb::Parameters::new();
    parameters.insert("p".to_owned(), int(1));
    let result = db
        .execute("MATCH (n:Missing) WHERE $p RETURN n", &parameters)
        .unwrap();
    assert!(result.rows().is_empty());
}

#[test]
fn a_searched_case_needs_a_boolean_or_null_condition() {
    let db = db();
    fails_before_reading_data(&db, "RETURN CASE WHEN 1 THEN 'x' ELSE 'y' END");
    fails_before_reading_data(&db, "MATCH (n:Missing) RETURN CASE WHEN 'a' THEN 1 END");
    assert_eq!(
        error_kind(
            &db,
            "UNWIND [1] AS x RETURN CASE WHEN x THEN 'x' ELSE 'y' END"
        ),
        ErrorKind::TypeMismatch
    );
    assert_eq!(
        run(
            &db,
            "UNWIND [true, false, null] AS x RETURN CASE WHEN x THEN 'x' ELSE 'y' END"
        ),
        [vec![string("x")], vec![string("y")], vec![string("y")]]
    );
    assert_eq!(
        single(&db, "RETURN CASE 1 WHEN 1 THEN 'x' ELSE 'y' END"),
        string("x")
    );
}

#[test]
fn a_union_part_may_delete_an_element_an_earlier_part_returned() {
    let db = db();
    run(&db, "CREATE (:A {v: 1})");
    let rows = run(
        &db,
        "MATCH (n) RETURN n AS x UNION ALL MATCH (n) DETACH DELETE n RETURN 1 AS x",
    );
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert!(matches!(rows[0][0], Value::Node(_)), "{rows:?}");
    assert_eq!(rows[1][0], int(1));
}

#[test]
fn a_function_argument_of_a_type_the_text_fixes_fails_before_reading_data() {
    let db = db();
    for query in [
        "MATCH (n) RETURN toUpper([1][0]) AS x",
        "MATCH (n) RETURN abs(toString(1)) AS x",
        "MATCH (n) RETURN [x IN [1, [1]] | toString(x)] AS x",
    ] {
        assert_eq!(
            error_kind(&db, query),
            ErrorKind::InvalidArgumentValue,
            "{query}"
        );
    }
    for query in [
        "MATCH (n) RETURN substring('abc', -1) AS x",
        "MATCH (n) RETURN range(1, 2, 0) AS x",
        "MATCH (n) RETURN abs(-9223372036854775807 - 1) AS x",
    ] {
        assert!(run(&db, query).is_empty(), "{query}");
    }
}

#[test]
fn split_by_the_empty_string_gives_one_string_per_character() {
    let db = db();
    assert_eq!(
        single(&db, "RETURN split('abc', '')"),
        list([string("a"), string("b"), string("c")])
    );
    assert_eq!(single(&db, "RETURN split('', '')"), list([]));
    assert_eq!(single(&db, "RETURN split('', ',')"), list([string("")]));
    assert_eq!(
        single(&db, "RETURN split('a,,b', ',')"),
        list([string("a"), string(""), string("b")])
    );
}

#[test]
fn an_operator_on_a_variable_of_a_kind_the_text_fixes_fails_before_reading_data() {
    let db = db();
    for query in [
        "MATCH (n) RETURN -n",
        "MATCH (n) RETURN n + 1",
        "MATCH (n) RETURN NOT n",
        "MATCH p = ()-->() RETURN p:T",
        "MATCH p = ()-->() RETURN p[0..1]",
    ] {
        fails_before_reading_data(&db, query);
    }
    assert_eq!(
        error_kind(&db, "MATCH (n) RETURN n[0]"),
        ErrorKind::MapElementAccessByNonString
    );
    run(&db, "CREATE (:A {x: 1})-[:T]->()");
    assert_eq!(
        run(&db, "MATCH (n:A) RETURN size(n + [1]) AS a, n['x'] AS b"),
        [vec![int(2), int(1)]]
    );
}

#[test]
fn a_full_exists_subquery_runs_from_the_outer_row_and_keeps_its_variables_inside() {
    let db = db();
    run(
        &db,
        "CREATE (a:A {i: 1})-[:R]->(:B {i: 1}), (a)-[:R]->(:B {i: 2}), (:A {i: 3})",
    );
    assert_eq!(
        run(
            &db,
            "MATCH (n:A) WHERE EXISTS { MATCH (n)-->(m) WITH count(m) AS c WHERE c = 2 RETURN c } \
             RETURN n.i",
        ),
        [vec![int(1)]]
    );
    assert_eq!(
        run(
            &db,
            "MATCH (n:A) WHERE EXISTS { MATCH (n)-->(m) WHERE m.i = n.i } RETURN n.i",
        ),
        [vec![int(1)]]
    );
    assert_eq!(
        error_kind(&db, "MATCH (n:A) WHERE EXISTS { MATCH (n)-->(m) } RETURN m"),
        ErrorKind::UndefinedVariable
    );
    assert_eq!(
        error_kind(
            &db,
            "MATCH (n:A) WHERE EXISTS { MATCH (n)-->(m) SET m.i = 0 } RETURN n"
        ),
        ErrorKind::InvalidClauseComposition
    );
}

#[test]
fn a_value_error_in_the_query_text_does_not_hide_a_later_type_error() {
    let db = db();
    for query in [
        "MATCH (n:Missing) RETURN [x IN [0, 'a'] | 1 / x] AS x",
        "MATCH (n:Missing) RETURN [x IN [0, 1] | (1 / x) + [x IN ['a'] | -x][0]] AS x",
        "MATCH (n:Missing) RETURN [x IN [9223372036854775807, 'a'] | x + 1 - true] AS x",
    ] {
        fails_before_reading_data(&db, query);
    }
    assert!(run(&db, "MATCH (n:Missing) RETURN [x IN [0] | 1 / x] AS x").is_empty());
    for query in [
        "MATCH (n:Missing) RETURN coalesce(1 / 0, 'a') - 1 AS v",
        "MATCH (n:Missing) RETURN [x IN [coalesce(1 / 0, 'a')] | x - 1] AS v",
        "MATCH (n:Missing) RETURN CASE WHEN coalesce(1 / 0, 1) THEN 1 END AS v",
    ] {
        assert!(run(&db, query).is_empty(), "{query}");
    }
    assert_eq!(
        error_kind(&db, "RETURN coalesce(1 / 0, 'a') - 1 AS v"),
        ErrorKind::DivisionByZero
    );
    assert_eq!(
        error_kind(&db, "RETURN [x IN [0, 'a'] | 1 / x] AS x"),
        ErrorKind::InvalidArgumentType
    );
}

#[test]
fn an_operand_that_may_be_null_still_fails_early_when_no_value_of_its_type_fits() {
    let db = db();
    run(&db, "CREATE (:A)");
    for query in [
        "MATCH (n:A) RETURN (n.x = 1) + 1",
        "MATCH (n:A) RETURN (n.x STARTS WITH 'a') AND 1",
        "OPTIONAL MATCH (n:Missing) RETURN n + 1",
    ] {
        fails_before_reading_data(&db, query);
    }
    assert_eq!(
        single(&db, "MATCH (n:A) WITH n.x = 1 AS b RETURN b + 1"),
        Value::Null
    );
}

#[test]
fn both_exists_forms_reject_a_variable_hidden_by_the_projection_next_to_them() {
    let db = db();
    run(&db, "CREATE (:C {x: 1})-[:R]->()");
    for query in [
        "MATCH (n:C) WITH DISTINCT n.x AS k ORDER BY EXISTS { (n)-->() } RETURN k",
        "MATCH (n:C) WITH DISTINCT n.x AS k ORDER BY EXISTS { MATCH (n)-->() } RETURN k",
        "MATCH (n:C) RETURN count(*) AS c ORDER BY EXISTS { MATCH (n)-->() }",
        "MATCH (n:C) WITH DISTINCT n.x AS k ORDER BY EXISTS { UNWIND [n] AS m RETURN m } RETURN k",
    ] {
        assert_eq!(
            error_kind(&db, query),
            ErrorKind::UndefinedVariable,
            "{query}"
        );
    }
    assert_eq!(
        run(
            &db,
            "MATCH (n:C) WITH DISTINCT n.x AS k ORDER BY EXISTS { MATCH (m)-->() } RETURN k"
        ),
        [vec![int(1)]]
    );
    assert_eq!(
        run(
            &db,
            "MATCH (n:C) WITH DISTINCT n.x AS k \
             ORDER BY EXISTS { MATCH (m) WITH m AS n MATCH (n)-->() } RETURN k"
        ),
        [vec![int(1)]]
    );
}
