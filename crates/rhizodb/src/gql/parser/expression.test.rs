use crate::gql::ast::BinaryOp;
use crate::gql::ast::ComparisonOp;
use crate::gql::ast::ExistsSubquery;
use crate::gql::ast::Expr;
use crate::gql::ast::Literal;
use crate::gql::ast::Name;
use crate::gql::ast::Quantifier;
use crate::gql::error::SyntaxErrorKind;
use crate::gql::testing::expr;
use crate::gql::testing::expr_error;
use crate::gql::testing::sexp;

fn assert_shapes(cases: &[(&str, &str)]) {
    for (source, expected) in cases {
        assert_eq!(sexp(&expr(source)), *expected, "{source}");
    }
}

fn name(text: &str) -> Name {
    Name::new(text)
}

#[test]
fn integer_literals_in_three_radixes() {
    assert_shapes(&[
        ("42", "42"),
        ("0x1F", "31"),
        ("0o17", "15"),
        ("9223372036854775807", "9223372036854775807"),
    ]);
}

#[test]
fn float_literals_with_fractions_and_exponents() {
    assert_shapes(&[
        ("1.5", "1.5"),
        ("1.5e3", "1500.0"),
        (".5", "0.5"),
        ("2E-2", "0.02"),
    ]);
}

#[test]
fn string_literals_decode_escapes_in_both_quote_styles() {
    assert_eq!(
        expr(r#"'it\'s'"#),
        Expr::Literal(Literal::String("it's".into()))
    );
    assert_eq!(
        expr(r#""tab\tAB""#),
        Expr::Literal(Literal::String("tab\tAB".into()))
    );
}

#[test]
fn boolean_and_null_literals_ignore_keyword_case() {
    assert_eq!(expr("true"), Expr::Literal(Literal::Boolean(true)));
    assert_eq!(expr("FALSE"), Expr::Literal(Literal::Boolean(false)));
    assert_eq!(expr("Null"), Expr::Literal(Literal::Null));
}

#[test]
fn a_minus_before_a_number_makes_a_negative_literal() {
    assert_eq!(expr("-5"), Expr::Literal(Literal::Integer(-5)));
    assert_eq!(expr("-2.5"), Expr::Literal(Literal::Float(-2.5)));
    assert_eq!(expr("- 5"), Expr::Literal(Literal::Integer(-5)));
    assert_eq!(expr("- -5"), Expr::Literal(Literal::Integer(5)));
}

#[test]
fn the_smallest_integer_is_a_valid_literal() {
    assert_eq!(
        expr("-9223372036854775808"),
        Expr::Literal(Literal::Integer(i64::MIN))
    );
}

#[test]
fn a_minus_before_a_non_literal_stays_an_operator() {
    assert_shapes(&[
        ("-a", "(neg a)"),
        ("+a", "(pos a)"),
        ("-(a + 1)", "(neg (+ a 1))"),
    ]);
}

#[test]
fn integers_past_64_bits_report_overflow_at_the_literal() {
    for (source, offset) in [
        ("9223372036854775808", 0),
        ("0xFFFFFFFFFFFFFFFF", 0),
        ("-9223372036854775809", 1),
        ("1 + 9223372036854775808", 4),
    ] {
        assert_eq!(
            expr_error(source),
            (offset, SyntaxErrorKind::IntegerOverflow),
            "{source}"
        );
    }
}

#[test]
fn arithmetic_precedence_and_associativity() {
    assert_shapes(&[
        ("1 + 2 * 3", "(+ 1 (* 2 3))"),
        ("1 * 2 + 3", "(+ (* 1 2) 3)"),
        ("(1 + 2) * 3", "(* (+ 1 2) 3)"),
        ("1 - 2 - 3", "(- (- 1 2) 3)"),
        ("8 / 4 / 2", "(/ (/ 8 4) 2)"),
        ("a % b * c", "(* (% a b) c)"),
        ("2 ^ 3 ^ 2", "(^ (^ 2 3) 2)"),
        ("2 * 3 ^ 2", "(* 2 (^ 3 2))"),
        ("-a ^ 2", "(^ (neg a) 2)"),
        ("1 - -1", "(- 1 -1)"),
        ("1--1", "(- 1 -1)"),
    ]);
}

#[test]
fn boolean_operators_bind_not_then_and_then_xor_then_or() {
    assert_shapes(&[
        ("a OR b AND c", "(or a (and b c))"),
        ("a OR b XOR c", "(or a (xor b c))"),
        ("a XOR b AND c", "(xor a (and b c))"),
        ("a AND b AND c", "(and (and a b) c)"),
        ("NOT a AND b", "(and (not a) b)"),
        ("NOT a = b", "(not (cmp a = b))"),
        ("NOT NOT a", "(not (not a))"),
        ("a AND NOT b", "(and a (not b))"),
    ]);
}

#[test]
fn comparisons_sit_between_boolean_and_arithmetic_operators() {
    assert_shapes(&[
        ("a = b AND c <> d", "(and (cmp a = b) (cmp c <> d))"),
        ("a + 1 < b * 2", "(cmp (+ a 1) < (* b 2))"),
        ("a >= 1", "(cmp a >= 1)"),
        ("a <= 1", "(cmp a <= 1)"),
        ("a > 1", "(cmp a > 1)"),
    ]);
}

#[test]
fn chained_comparisons_form_one_chain_but_parentheses_break_it() {
    assert_shapes(&[
        ("1 < a <= 3", "(cmp 1 < a <= 3)"),
        ("a = b = c", "(cmp a = b = c)"),
        ("(1 < a) < 3", "(cmp (cmp 1 < a) < 3)"),
    ]);
    assert!(matches!(
        expr("1 < a < 3"),
        Expr::Comparison { rest, .. } if rest.len() == 2 && rest[0].0 == ComparisonOp::Less
    ));
}

#[test]
fn predicates_share_the_comparison_level() {
    assert_shapes(&[
        ("a IN b + c", "(in a (+ b c))"),
        ("a + b IN c", "(in (+ a b) c)"),
        ("a.x STARTS WITH 'p' AND b", "(and (starts (. a x) 'p') b)"),
        ("a ENDS WITH 's'", "(ends a 's')"),
        ("a CONTAINS 's'", "(contains a 's')"),
        ("a =~ 'x.*'", "(=~ a 'x.*')"),
        (
            "a IS NULL AND b IS NOT NULL",
            "(and (is-null a) (is-not-null b))",
        ),
        ("a + 1 IS NULL", "(is-null (+ a 1))"),
    ]);
}

#[test]
fn keywords_are_case_insensitive_and_names_are_not() {
    assert_shapes(&[
        ("a and B Or c", "(or (and a B) c)"),
        ("x StArTs WiTh 'a'", "(starts x 'a')"),
        ("x is not null", "(is-not-null x)"),
    ]);
    assert_ne!(expr("a"), expr("A"));
}

#[test]
fn variables_parameters_and_quoted_names() {
    assert_eq!(expr("n"), Expr::Variable(name("n")));
    assert_eq!(expr("`my var`"), Expr::Variable(name("my var")));
    assert_eq!(expr("`RETURN`"), Expr::Variable(name("RETURN")));
    assert_eq!(expr("$limit"), Expr::Parameter(name("limit")));
    assert_eq!(expr("$0"), Expr::Parameter(name("0")));
}

#[test]
fn property_access_chains() {
    assert_shapes(&[
        ("n.name", "(. n name)"),
        ("n.a.b", "(. (. n a) b)"),
        ("n.`a b`", "(. n a b)"),
        ("n.type", "(. n type)"),
    ]);
}

#[test]
fn indexing_and_slicing() {
    assert_shapes(&[
        ("l[0]", "(index l 0)"),
        ("l[1..3]", "(slice l 1 3)"),
        ("l[1..]", "(slice l 1 _)"),
        ("l[..3]", "(slice l _ 3)"),
        ("l[..]", "(slice l _ _)"),
        ("l[-1]", "(index l -1)"),
        ("n.xs[0].y", "(. (index (. n xs) 0) y)"),
        ("[1, 2][0]", "(index [1 2] 0)"),
    ]);
}

#[test]
fn label_predicates() {
    assert_shapes(&[("n:A", "(labels n :A)"), ("n:A:B", "(labels n :A:B)")]);
}

#[test]
fn list_and_map_literals() {
    assert_shapes(&[
        ("[]", "[]"),
        ("[1, 'a', [2]]", "[1 'a' [2]]"),
        ("{}", "{}"),
        ("{a: 1, `b c`: {d: null}}", "{a:1 b c:{d:null}}"),
        ("{type: 1}", "{type:1}"),
    ]);
}

#[test]
fn function_calls_with_namespaces_and_distinct() {
    assert_shapes(&[
        ("f()", "(call f )"),
        ("f(1, a)", "(call f 1 a)"),
        ("apoc.text.join(a)", "(call apoc.text.join a)"),
        ("count(DISTINCT n.x)", "(call count distinct (. n x))"),
        ("COUNT(*)", "count(*)"),
        ("count(n)", "(call count n)"),
        ("toUpper(a) + 1", "(+ (call toUpper a) 1)"),
    ]);
}

#[test]
fn searched_and_simple_case() {
    assert_shapes(&[
        (
            "CASE WHEN a THEN 1 WHEN b THEN 2 ELSE 3 END",
            "(case [] (when a 1) (when b 2) else [3])",
        ),
        (
            "CASE x WHEN 1 THEN 'a' END",
            "(case [x] (when 1 'a') else [])",
        ),
        (
            "case when a then 1 end + 1",
            "(+ (case [] (when a 1) else []) 1)",
        ),
    ]);
}

#[test]
fn case_requires_a_when_branch_and_an_end() {
    assert_eq!(
        expr_error("CASE ELSE 1 END"),
        (5, SyntaxErrorKind::UnexpectedToken)
    );
    assert_eq!(
        expr_error("CASE WHEN a THEN 1"),
        (18, SyntaxErrorKind::UnexpectedEnd)
    );
}

#[test]
fn list_comprehension_with_every_part() {
    let Expr::ListComprehension(found) = expr("[x IN xs WHERE x > 1 | x * 2]") else {
        panic!("expected a list comprehension");
    };
    assert_eq!(found.variable, name("x"));
    assert_eq!(sexp(&found.source), "xs");
    assert_eq!(
        found.filter.as_deref().map(sexp).as_deref(),
        Some("(cmp x > 1)")
    );
    assert_eq!(
        found.projection.as_deref().map(sexp).as_deref(),
        Some("(* x 2)")
    );
}

#[test]
fn list_comprehension_parts_are_optional() {
    let Expr::ListComprehension(found) = expr("[x IN xs]") else {
        panic!("expected a list comprehension");
    };
    assert!(found.filter.is_none() && found.projection.is_none());
    let Expr::ListComprehension(found) = expr("[x IN xs | x]") else {
        panic!("expected a list comprehension");
    };
    assert!(found.filter.is_none() && found.projection.is_some());
}

#[test]
fn quantified_list_predicates() {
    for (source, quantifier) in [
        ("all(x IN xs WHERE x > 1)", Quantifier::All),
        ("ANY(x IN xs WHERE x)", Quantifier::Any),
        ("none(x IN xs WHERE x)", Quantifier::None),
        ("single(x IN xs WHERE x)", Quantifier::Single),
    ] {
        assert!(
            matches!(expr(source), Expr::ListPredicate(found) if found.quantifier == quantifier),
            "{source}"
        );
    }
}

#[test]
fn a_quantifier_without_where_is_rejected() {
    assert_eq!(
        expr_error("all(x IN xs)"),
        (11, SyntaxErrorKind::UnexpectedToken)
    );
}

#[test]
fn pattern_predicates_parse_as_patterns() {
    assert!(matches!(expr("(a)-->(b)"), Expr::Pattern(found) if found.steps.len() == 1));
    assert!(
        matches!(expr("(a)<-[:T]-(b)-[:U]->()"), Expr::Pattern(found) if found.steps.len() == 2)
    );
    assert!(matches!(
        expr("NOT (a)--(b)"),
        Expr::Unary { operand, .. } if matches!(*operand, Expr::Pattern(_))
    ));
}

#[test]
fn parentheses_around_a_variable_or_arithmetic_are_not_patterns() {
    assert_eq!(expr("(a)"), Expr::Variable(name("a")));
    assert_shapes(&[
        ("(a) - 1", "(- a 1)"),
        ("(a) - -1", "(- a -1)"),
        ("(a)-(b)", "(- a b)"),
    ]);
}

#[test]
fn pattern_comprehensions() {
    let Expr::PatternComprehension(found) = expr("[p = (a)-->(b) WHERE b.x > 1 | b.name]") else {
        panic!("expected a pattern comprehension");
    };
    assert_eq!(found.path_variable, Some(name("p")));
    assert_eq!(found.element.steps.len(), 1);
    assert!(found.filter.is_some());
    assert!(
        matches!(expr("[(a)-[:T]->(b) | b]"), Expr::PatternComprehension(c) if c.path_variable.is_none())
    );
}

#[test]
fn a_list_starting_with_a_parenthesized_expression_is_still_a_list() {
    assert_shapes(&[("[(1 + 2), 3]", "[(+ 1 2) 3]")]);
}

#[test]
fn existential_subqueries() {
    assert!(matches!(
        expr("exists { (n)-->() WHERE n.x = 1 }"),
        Expr::Exists(body) if matches!(*body, ExistsSubquery::Pattern { filter: Some(_), .. })
    ));
    assert!(matches!(
        expr("EXISTS { MATCH (n)-->() RETURN true }"),
        Expr::Exists(body) if matches!(*body, ExistsSubquery::Query(_))
    ));
}

#[test]
fn exists_as_a_plain_function_still_works() {
    assert_shapes(&[("exists(n.x)", "(call exists (. n x))")]);
}

#[test]
fn binary_operator_enum_matches_the_source_operator() {
    assert!(matches!(
        expr("a XOR b"),
        Expr::Binary {
            op: BinaryOp::Xor,
            ..
        }
    ));
    assert!(matches!(
        expr("a ^ b"),
        Expr::Binary {
            op: BinaryOp::Power,
            ..
        }
    ));
}

#[test]
fn malformed_expressions_report_the_offending_offset() {
    for (source, expected) in [
        ("1 +", (3, SyntaxErrorKind::UnexpectedEnd)),
        ("(1 + 2", (6, SyntaxErrorKind::UnexpectedEnd)),
        ("1 + * 2", (4, SyntaxErrorKind::UnexpectedToken)),
        ("[1, 2", (5, SyntaxErrorKind::UnexpectedEnd)),
        ("{a 1}", (3, SyntaxErrorKind::UnexpectedToken)),
        ("{a: 1,}", (6, SyntaxErrorKind::UnexpectedToken)),
        ("l[]", (2, SyntaxErrorKind::UnexpectedToken)),
        ("a IS 1", (5, SyntaxErrorKind::UnexpectedToken)),
        ("a STARTS 1", (2, SyntaxErrorKind::UnexpectedToken)),
        ("f(1,)", (4, SyntaxErrorKind::UnexpectedToken)),
        ("n.", (2, SyntaxErrorKind::UnexpectedEnd)),
    ] {
        assert_eq!(expr_error(source), expected, "{source}");
    }
}

#[test]
fn lexical_errors_surface_through_the_parser() {
    assert_eq!(
        expr_error("1 + 0xZ"),
        (4, SyntaxErrorKind::InvalidNumberLiteral)
    );
    assert_eq!(expr_error(r"'\q'"), (1, SyntaxErrorKind::InvalidEscape));
    assert_eq!(
        expr_error(r"'\u00G0'"),
        (1, SyntaxErrorKind::InvalidUnicodeEscape)
    );
}
