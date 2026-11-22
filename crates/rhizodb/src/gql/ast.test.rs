use std::convert::Infallible;

use super::LabelExpr;
use super::Name;

fn label(text: &str) -> LabelExpr {
    LabelExpr::Name(Name::new(text))
}

fn not(operand: LabelExpr) -> LabelExpr {
    LabelExpr::Not(Box::new(operand))
}

fn matches(expr: &LabelExpr, labels: &[&str]) -> bool {
    let result: Result<bool, Infallible> =
        expr.matches(!labels.is_empty(), &mut |name| Ok(labels.contains(&name)));
    result.unwrap_or_else(|never| match never {})
}

#[test]
fn each_operator_combines_its_operands() {
    let a_and_b = LabelExpr::And(vec![label("A"), label("B")]);
    let a_or_b = LabelExpr::Or(vec![label("A"), label("B")]);
    for (labels, and, or, not_a) in [
        (&["A", "B"][..], true, true, false),
        (&["A"][..], false, true, false),
        (&["C"][..], false, false, true),
        (&[][..], false, false, true),
    ] {
        assert_eq!(matches(&a_and_b, labels), and, "{labels:?}");
        assert_eq!(matches(&a_or_b, labels), or, "{labels:?}");
        assert_eq!(matches(&not(label("A")), labels), not_a, "{labels:?}");
    }
}

#[test]
fn the_wildcard_needs_some_label() {
    assert!(matches(&LabelExpr::Wildcard, &["X"]));
    assert!(!matches(&LabelExpr::Wildcard, &[]));
    assert!(matches(&not(LabelExpr::Wildcard), &[]));
}

#[test]
fn an_error_from_the_label_test_stops_the_match() {
    let expr = LabelExpr::Or(vec![label("A"), label("B")]);
    let mut asked = Vec::new();
    let result = expr.matches(true, &mut |name| {
        asked.push(name.to_owned());
        Err(())
    });
    assert_eq!(result, Err(()));
    assert_eq!(asked, ["A"]);
}

#[test]
fn names_lists_every_label_the_expression_mentions() {
    let expr = LabelExpr::Or(vec![
        LabelExpr::And(vec![label("A"), not(label("B"))]),
        LabelExpr::Wildcard,
        label("A"),
    ]);
    let names: Vec<&str> = expr.names().iter().map(|name| name.0.as_str()).collect();
    assert_eq!(names, ["A", "B", "A"]);
}

#[test]
fn a_conjunction_requires_its_first_plain_label() {
    let expr = LabelExpr::And(vec![not(label("A")), label("B"), label("C")]);
    assert_eq!(expr.required(), Some(&Name::new("B")));
    assert_eq!(LabelExpr::Or(vec![label("A"), label("B")]).required(), None);
    assert_eq!(not(label("A")).required(), None);
}

#[test]
fn only_names_and_their_conjunctions_form_a_label_set() {
    let a = Name::new("A");
    let b = Name::new("B");
    assert_eq!(label("A").label_set(), Some(vec![&a]));
    assert_eq!(
        LabelExpr::And(vec![label("A"), label("B")]).label_set(),
        Some(vec![&a, &b])
    );
    for expr in [
        LabelExpr::Wildcard,
        not(label("A")),
        LabelExpr::Or(vec![label("A"), label("B")]),
        LabelExpr::And(vec![label("A"), not(label("B"))]),
    ] {
        assert_eq!(expr.label_set(), None, "{expr:?}");
    }
}
