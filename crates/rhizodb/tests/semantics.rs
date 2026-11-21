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
