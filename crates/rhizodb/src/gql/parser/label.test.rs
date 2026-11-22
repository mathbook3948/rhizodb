use crate::gql::ast::Expr;
use crate::gql::ast::LabelExpr;
use crate::gql::ast::ListComprehension;
use crate::gql::ast::Name;
use crate::gql::error::SyntaxErrorKind;
use crate::gql::testing::error;
use crate::gql::testing::expr;
use crate::gql::testing::label_sexp;
use crate::gql::testing::patterns;
use crate::gql::testing::predicate;
use crate::gql::testing::sexp;

fn label(text: &str) -> LabelExpr {
    LabelExpr::Name(Name::new(text))
}

fn node_labels(source: &str) -> String {
    let parts = patterns(source);
    parts[0]
        .element
        .start
        .labels
        .as_ref()
        .map_or("_".into(), label_sexp)
}

fn relationship_labels(source: &str) -> String {
    let parts = patterns(source);
    parts[0].element.steps[0]
        .relationship
        .labels
        .as_ref()
        .map_or("_".into(), label_sexp)
}

#[test]
fn node_patterns_take_every_label_operator() {
    for (source, expected) in [
        ("(n)", "_"),
        ("(n:A)", "A"),
        ("(n:A&B)", "(& A B)"),
        ("(n:A|B)", "(| A B)"),
        ("(n:!A)", "(! A)"),
        ("(n:%)", "%"),
        ("(n:!%)", "(! %)"),
        ("(n:A|B&C)", "(| A (& B C))"),
        ("(n:!A&B)", "(& (! A) B)"),
        ("(n:(A|B)&C)", "(& (| A B) C)"),
        ("(n:A|:B)", "(| A B)"),
        ("(n:A:B:C)", "(& A B C)"),
    ] {
        assert_eq!(node_labels(source), expected, "{source}");
    }
}

#[test]
fn is_introduces_labels_with_or_without_a_variable() {
    let parts = patterns("(n IS A&B)");
    assert_eq!(
        parts[0]
            .element
            .start
            .variable
            .as_ref()
            .map(|n| n.0.as_str()),
        Some("n")
    );
    assert_eq!(node_labels("(n IS A&B)"), "(& A B)");
    let parts = patterns("(IS A)");
    assert_eq!(parts[0].element.start.variable, None);
    assert_eq!(node_labels("(is A)"), "A");
}

#[test]
fn a_variable_named_is_stays_a_variable() {
    for source in ["(is)", "(is:A)", "(is {k: 1})"] {
        let parts = patterns(source);
        assert_eq!(
            parts[0]
                .element
                .start
                .variable
                .as_ref()
                .map(|n| n.0.as_str()),
            Some("is"),
            "{source}"
        );
    }
}

#[test]
fn a_variable_named_is_takes_an_is_label_expression() {
    let parts = patterns("(is IS Person)");
    let start = &parts[0].element.start;
    assert_eq!(start.variable.as_ref().map(|n| n.0.as_str()), Some("is"));
    assert_eq!(node_labels("(is IS Person)"), "Person");
    let parts = patterns("()-[is IS KNOWS]->()");
    let relationship = &parts[0].element.steps[0].relationship;
    assert_eq!(
        relationship.variable.as_ref().map(|n| n.0.as_str()),
        Some("is")
    );
    assert_eq!(relationship_labels("()-[is IS KNOWS]->()"), "KNOWS");
    assert_eq!(node_labels("(IS A)"), "A");
    assert_eq!(node_labels("(is:A)"), "A");
}

#[test]
fn relationship_patterns_take_one_label_expression() {
    for (source, expected) in [
        ("()-[]->()", "_"),
        ("()-[:T]->()", "T"),
        ("()-[:T|U]->()", "(| T U)"),
        ("()-[:T|:U]->()", "(| T U)"),
        ("()-[r:!T]->()", "(! T)"),
        ("()-[r IS T|U]->()", "(| T U)"),
        ("()-[IS T*1..2]->()", "T"),
        ("()-[:T&U {k: 1}]->()", "(& T U)"),
    ] {
        assert_eq!(relationship_labels(source), expected, "{source}");
    }
}

#[test]
fn label_tests_in_expressions_take_every_operator() {
    for (source, expected) in [
        ("n:A", "(labels n A)"),
        ("n:A:B", "(labels n (& A B))"),
        ("n:A&!B", "(labels n (& A (! B)))"),
        ("n:A|B", "(labels n (| A B))"),
        ("n:(A|B)", "(labels n (| A B))"),
        ("n:%", "(labels n %)"),
        ("n IS LABELED A|B", "(labels n (| A B))"),
        ("n is labeled !A", "(labels n (! A))"),
        ("n IS NOT LABELED A", "(not (labels n A))"),
    ] {
        assert_eq!(sexp(&expr(source)), expected, "{source}");
    }
}

#[test]
fn a_labeled_predicate_binds_like_is_null() {
    assert_eq!(
        sexp(&predicate("a IS LABELED A AND NOT a IS LABELED B")),
        "(and (labels a A) (not (labels a B)))"
    );
    assert_eq!(
        sexp(&predicate("a:A = b:B")),
        "(cmp (labels a A) = (labels b B))"
    );
}

fn comprehension(source: &str) -> ListComprehension {
    match expr(source) {
        Expr::ListComprehension(comprehension) => comprehension,
        other => panic!("expected a list comprehension in {source}, got {other:?}"),
    }
}

#[test]
fn a_pipe_in_a_comprehension_filter_starts_the_projection() {
    for (source, filter, projection) in [
        ("[x IN l WHERE x:A | x]", "(labels x A)", "x"),
        (
            "[x IN l WHERE x IS LABELED A | x.k]",
            "(labels x A)",
            "(. x k)",
        ),
        ("[x IN l WHERE (x:A|B) | x]", "(labels x (| A B))", "x"),
        ("[x IN l WHERE x:(A|B) | x]", "(labels x (| A B))", "x"),
        (
            "[x IN l WHERE x IN [y:A|B] | x]",
            "(in x [(labels y (| A B))])",
            "x",
        ),
        ("[x IN l | x:A|B]", "_", "(labels x (| A B))"),
    ] {
        let found = comprehension(source);
        assert_eq!(
            found.filter.as_deref().map_or("_".into(), sexp),
            filter,
            "{source}"
        );
        assert_eq!(
            found.projection.as_deref().map_or("_".into(), sexp),
            projection,
            "{source}"
        );
    }
}

#[test]
fn a_delimited_part_of_a_comprehension_filter_keeps_its_disjunction() {
    for source in [
        "[x IN l WHERE coalesce(x:A|B, false) | x]",
        "[x IN l WHERE CASE WHEN x:A|B THEN true END | x]",
        "[x IN l WHERE CASE x:A|B WHEN true THEN x IS LABELED A|B ELSE x:A|B END | x]",
        "[x IN l WHERE {k: x:A|B}.k | x]",
        "[x IN l WHERE m[x:A|B] | x]",
        "[x IN l WHERE m[x:A|B..x:A|B] | x]",
        "[x IN l WHERE EXISTS { MATCH (x) WHERE x:A|B } | x]",
    ] {
        let found = comprehension(source);
        let filter = format!("{:?}", found.filter);
        let disjunction = format!("{:?}", LabelExpr::Or(vec![label("A"), label("B")]));
        assert!(filter.contains(&disjunction), "{source}: {filter}");
        assert_eq!(
            found.projection.as_deref().map(sexp).as_deref(),
            Some("x"),
            "{source}"
        );
    }
}

#[test]
fn a_pattern_in_a_comprehension_filter_keeps_its_disjunction() {
    let found = comprehension("[x IN l WHERE (x)-[:T|U]->(:A|B) | x]");
    let Some(Expr::Pattern(element)) = found.filter.as_deref() else {
        panic!("expected a pattern predicate, got {found:?}");
    };
    let labels = element.steps[0].relationship.labels.as_ref();
    assert_eq!(labels.map(label_sexp).as_deref(), Some("(| T U)"));
    let labels = element.steps[0].node.labels.as_ref();
    assert_eq!(labels.map(label_sexp).as_deref(), Some("(| A B)"));
}

#[test]
fn label_expression_errors_report_offsets() {
    for (source, expected) in [
        (
            "MATCH (n:A&) RETURN n",
            (11, SyntaxErrorKind::UnexpectedToken),
        ),
        (
            "MATCH (n:A|) RETURN n",
            (11, SyntaxErrorKind::UnexpectedToken),
        ),
        (
            "MATCH (n:!) RETURN n",
            (10, SyntaxErrorKind::UnexpectedToken),
        ),
        (
            "MATCH (n:!!A) RETURN n",
            (10, SyntaxErrorKind::UnexpectedToken),
        ),
        ("MATCH (n:) RETURN n", (9, SyntaxErrorKind::UnexpectedToken)),
        (
            "MATCH (n:(A|B) RETURN n",
            (15, SyntaxErrorKind::UnexpectedToken),
        ),
        (
            "MATCH (n:A&&B) RETURN n",
            (11, SyntaxErrorKind::UnexpectedToken),
        ),
        (
            "MATCH (n IS) RETURN n",
            (11, SyntaxErrorKind::UnexpectedToken),
        ),
        (
            "MATCH (n IS A:B) RETURN n",
            (13, SyntaxErrorKind::UnexpectedToken),
        ),
        (
            "MATCH (n:A|B:C) RETURN n",
            (12, SyntaxErrorKind::UnexpectedToken),
        ),
        (
            "MATCH (n:A:B|C) RETURN n",
            (10, SyntaxErrorKind::UnexpectedToken),
        ),
        (
            "MATCH (n:A:!B) RETURN n",
            (10, SyntaxErrorKind::UnexpectedToken),
        ),
        (
            "MATCH ()-[:T:U]->() RETURN 1",
            (12, SyntaxErrorKind::UnexpectedToken),
        ),
        (
            "MATCH ()-[r IS]->() RETURN 1",
            (14, SyntaxErrorKind::UnexpectedToken),
        ),
        ("RETURN n IS LABELED", (19, SyntaxErrorKind::UnexpectedEnd)),
        (
            "RETURN n IS LABELED 1",
            (20, SyntaxErrorKind::UnexpectedToken),
        ),
        ("RETURN n IS NOT A", (16, SyntaxErrorKind::UnexpectedToken)),
        ("RETURN n:A|B:C", (12, SyntaxErrorKind::UnexpectedToken)),
    ] {
        assert_eq!(error(source), expected, "{source:?}");
    }
}

#[test]
fn deeply_parenthesized_labels_are_an_error_not_a_stack_overflow() {
    let source = format!(
        "MATCH (n:{}A{}) RETURN n",
        "(".repeat(5000),
        ")".repeat(5000)
    );
    assert_eq!(error(&source).1, SyntaxErrorKind::NestingTooDeep);
}
