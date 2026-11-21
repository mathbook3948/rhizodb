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
    ] {
        assert_eq!(error_kind(&db, query), kind, "{query}");
    }
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
fn reading_a_deleted_element_fails() {
    let db = db();
    run(&db, "CREATE ({x: 1})");
    assert_eq!(
        error_kind(&db, "MATCH (n) DELETE n RETURN n.x"),
        ErrorKind::DeletedEntityAccess
    );
}

#[test]
fn an_expression_conditional_evaluation_skips_never_fails() {
    let db = db();
    assert!(
        run(
            &db,
            "MATCH (n) WHERE false RETURN CASE WHEN n IS NULL THEN 0 ELSE -'x' END"
        )
        .is_empty()
    );
    run(&db, "CREATE (:N)");
    assert_eq!(
        single(
            &db,
            "MATCH (n:N) RETURN CASE WHEN n IS NOT NULL THEN 0 ELSE -'x' END"
        ),
        int(0)
    );
    assert_eq!(
        single(&db, "RETURN CASE WHEN true THEN 1 ELSE -'x' END"),
        int(1)
    );
    assert_eq!(
        single(&db, "RETURN [x IN [] | -'x']"),
        Value::List(Vec::new())
    );
    assert_eq!(
        error_kind(
            &db,
            "MATCH (n:N) RETURN CASE WHEN n IS NULL THEN 0 ELSE -'x' END"
        ),
        ErrorKind::TypeMismatch
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
