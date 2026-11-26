use crate::gql::ast::Direction;
use crate::gql::ast::Expr;
use crate::gql::ast::LabelExpr;
use crate::gql::ast::Length;
use crate::gql::ast::Literal;
use crate::gql::ast::Name;
use crate::gql::ast::NodePattern;
use crate::gql::ast::PathMode;
use crate::gql::ast::PathSelector;
use crate::gql::ast::PatternPart;
use crate::gql::ast::PatternStep;
use crate::gql::ast::Properties;
use crate::gql::ast::RelationshipPattern;
use crate::gql::error::SyntaxErrorKind;
use crate::gql::testing::error;
use crate::gql::testing::patterns;
use crate::gql::testing::predicate;

fn name(text: &str) -> Name {
    Name::new(text)
}

fn labels(names: &[&str]) -> Option<LabelExpr> {
    let mut operands: Vec<LabelExpr> = names
        .iter()
        .map(|label| LabelExpr::Name(name(label)))
        .collect();
    match operands.len() {
        0 => None,
        1 => operands.pop(),
        _ => Some(LabelExpr::And(operands)),
    }
}

fn node(variable: Option<&str>, names: &[&str]) -> NodePattern {
    NodePattern {
        variable: variable.map(name),
        labels: labels(names),
        properties: None,
    }
}

fn only_part(source: &str) -> PatternPart {
    let mut parts = patterns(source);
    assert_eq!(parts.len(), 1, "{source}");
    parts.remove(0)
}

fn only_relationship(source: &str) -> RelationshipPattern {
    let mut part = only_part(source);
    assert_eq!(part.element.steps.len(), 1, "{source}");
    part.element.steps.remove(0).edge().unwrap().0.clone()
}

#[test]
fn bare_and_decorated_nodes() {
    assert_eq!(only_part("()").element.start, node(None, &[]));
    assert_eq!(only_part("(n)").element.start, node(Some("n"), &[]));
    assert_eq!(only_part("(:A)").element.start, node(None, &["A"]));
    assert_eq!(
        only_part("(n:A:B)").element.start,
        node(Some("n"), &["A", "B"])
    );
    assert_eq!(
        only_part("(`my n`:`My Label`)").element.start,
        node(Some("my n"), &["My Label"])
    );
}

#[test]
fn node_properties_as_a_map_or_a_parameter() {
    assert_eq!(
        only_part("(n {name: 'a', age: 3})")
            .element
            .start
            .properties,
        Some(Properties::Map(vec![
            (name("name"), Expr::Literal(Literal::String("a".into()))),
            (name("age"), Expr::Literal(Literal::Integer(3))),
        ]))
    );
    assert_eq!(
        only_part("(n {})").element.start.properties,
        Some(Properties::Map(vec![]))
    );
    assert_eq!(
        only_part("(n $props)").element.start.properties,
        Some(Properties::Parameter(name("props")))
    );
}

#[test]
fn every_arrow_form_has_its_direction() {
    for (pattern, direction) in [
        ("(a)-->(b)", Direction::Outgoing),
        ("(a)<--(b)", Direction::Incoming),
        ("(a)--(b)", Direction::Any),
        ("(a)<-->(b)", Direction::IncomingOrOutgoing),
        ("(a)-[]->(b)", Direction::Outgoing),
        ("(a)<-[]-(b)", Direction::Incoming),
        ("(a)-[]-(b)", Direction::Any),
        ("(a)<-[]->(b)", Direction::IncomingOrOutgoing),
        ("(a)->(b)", Direction::Outgoing),
        ("(a)<-(b)", Direction::Incoming),
        ("(a)-(b)", Direction::Any),
        ("(a)<->(b)", Direction::IncomingOrOutgoing),
        ("(a)~[]~(b)", Direction::Undirected),
        ("(a)<~[]~(b)", Direction::IncomingOrUndirected),
        ("(a)~[]~>(b)", Direction::UndirectedOrOutgoing),
        ("(a)~(b)", Direction::Undirected),
        ("(a)<~(b)", Direction::IncomingOrUndirected),
        ("(a)~>(b)", Direction::UndirectedOrOutgoing),
        ("(a)~[r:T*1..2 {k: 1}]~(b)", Direction::Undirected),
    ] {
        assert_eq!(only_relationship(pattern).direction, direction, "{pattern}");
    }
}

#[test]
fn relationship_detail_variable_types_and_properties() {
    let found = only_relationship("(a)-[r:KNOWS|LIKES|:LOVES {since: 1}]->(b)");
    assert_eq!(found.variable, Some(name("r")));
    assert_eq!(
        found.labels,
        Some(LabelExpr::Or(vec![
            LabelExpr::Name(name("KNOWS")),
            LabelExpr::Name(name("LIKES")),
            LabelExpr::Name(name("LOVES")),
        ]))
    );
    assert_eq!(
        found.properties,
        Some(Properties::Map(vec![(
            name("since"),
            Expr::Literal(Literal::Integer(1))
        )]))
    );
    assert_eq!(found.length, None);
    assert_eq!(
        only_relationship("(a)-[:T $p]-(b)").properties,
        Some(Properties::Parameter(name("p")))
    );
}

#[test]
fn variable_length_bounds() {
    for (pattern, length) in [
        (
            "(a)-[*]->(b)",
            Length {
                min: None,
                max: None,
            },
        ),
        (
            "(a)-[*3]->(b)",
            Length {
                min: Some(3),
                max: Some(3),
            },
        ),
        (
            "(a)-[*1..3]->(b)",
            Length {
                min: Some(1),
                max: Some(3),
            },
        ),
        (
            "(a)-[*2..]->(b)",
            Length {
                min: Some(2),
                max: None,
            },
        ),
        (
            "(a)-[*..5]->(b)",
            Length {
                min: None,
                max: Some(5),
            },
        ),
        (
            "(a)-[*0..0]->(b)",
            Length {
                min: Some(0),
                max: Some(0),
            },
        ),
    ] {
        assert_eq!(only_relationship(pattern).length, Some(length), "{pattern}");
    }
}

#[test]
fn variable_length_with_types_and_variable_together() {
    let found = only_relationship("(a)-[r:T*1..2 {k: 1}]->(b)");
    assert_eq!(found.variable, Some(name("r")));
    assert_eq!(found.labels, labels(&["T"]));
    assert_eq!(
        found.length,
        Some(Length {
            min: Some(1),
            max: Some(2)
        })
    );
    assert!(found.properties.is_some());
}

#[test]
fn chains_keep_every_step_in_order() {
    let part = only_part("(a)-[:X]->(b:B)<-[:Y]-(c)--(d)");
    let directions: Vec<Direction> = part
        .element
        .steps
        .iter()
        .map(|s| s.edge().unwrap().0.direction)
        .collect();
    assert_eq!(
        directions,
        vec![Direction::Outgoing, Direction::Incoming, Direction::Any]
    );
    let found: Vec<Option<LabelExpr>> = part
        .element
        .steps
        .iter()
        .map(|s| s.edge().unwrap().1.labels.clone())
        .collect();
    assert_eq!(found, vec![labels(&["B"]), None, None]);
}

#[test]
fn comma_separated_patterns_and_named_paths() {
    let parts = patterns("p = (a)-->(b), (c), q = (d)");
    assert_eq!(parts.len(), 3);
    assert_eq!(parts[0].path_variable, Some(name("p")));
    assert_eq!(parts[1].path_variable, None);
    assert_eq!(parts[2].path_variable, Some(name("q")));
}

#[test]
fn path_search_prefixes_name_a_selector_and_a_mode() {
    for (source, selector, mode) in [
        (
            "ANY SHORTEST (a)-->(b)",
            Some(PathSelector::AnyShortest),
            None,
        ),
        (
            "ALL SHORTEST TRAIL (a)-->(b)",
            Some(PathSelector::AllShortest),
            Some(PathMode::Trail),
        ),
        ("ANY (a)-->(b)", Some(PathSelector::Any(1)), None),
        (
            "ANY 3 ACYCLIC PATHS (a)-->(b)",
            Some(PathSelector::Any(3)),
            Some(PathMode::Acyclic),
        ),
        (
            "SHORTEST 2 (a)-->(b)",
            Some(PathSelector::Shortest(2)),
            None,
        ),
        (
            "SHORTEST 2 SIMPLE PATH GROUPS (a)-->(b)",
            Some(PathSelector::ShortestGroups(2)),
            Some(PathMode::Simple),
        ),
        (
            "SHORTEST GROUP (a)-->(b)",
            Some(PathSelector::ShortestGroups(1)),
            None,
        ),
        (
            "ALL WALK (a)-->(b)",
            Some(PathSelector::All),
            Some(PathMode::Walk),
        ),
        ("TRAIL PATHS (a)-->(b)", None, Some(PathMode::Trail)),
        (
            "any shortest (a)-->(b)",
            Some(PathSelector::AnyShortest),
            None,
        ),
    ] {
        let search = only_part(source).search.unwrap();
        assert_eq!(
            (search.selector, search.mode, search.cypher),
            (selector, mode, false),
            "{source}"
        );
    }
}

#[test]
fn cypher_shortest_path_functions_read_as_selectors() {
    for (source, selector) in [
        ("p = shortestPath((a)-[*]-(b))", PathSelector::AnyShortest),
        ("allShortestPaths((a)-[*]-(b))", PathSelector::AllShortest),
    ] {
        let part = only_part(source);
        let search = part.search.unwrap();
        assert_eq!(
            (search.selector, search.cypher),
            (Some(selector), true),
            "{source}"
        );
        assert_eq!(part.element.steps.len(), 1, "{source}");
    }
}

#[test]
fn quantifiers_bound_edges_and_groups() {
    for (source, min, max) in [
        ("(a)-[:T]->{2,5}(b)", 2, Some(5)),
        ("(a)-[:T]->{3}(b)", 3, Some(3)),
        ("(a)->{,4}(b)", 0, Some(4)),
        ("(a)-->{1,}(b)", 1, None),
        ("(a)-->+(b)", 1, None),
        ("(a)-->*(b)", 0, None),
    ] {
        let length = only_relationship(source).length.unwrap();
        assert_eq!((length.min, length.max), (Some(min), max), "{source}");
    }
    let part = only_part("(a)((x)-[r]->(y) WHERE r.w > 1){1,3}(b)");
    let [PatternStep::Group { group, node }] = part.element.steps.as_slice() else {
        panic!("expected one group, got {:?}", part.element.steps);
    };
    assert_eq!((group.min, group.max), (1, Some(3)));
    assert!(group.filter.is_some());
    assert_eq!(group.element.steps.len(), 1);
    assert_eq!(node.variable, Some(name("b")));
    let part = only_part("((x)-->(y))+");
    assert_eq!(part.element.start, NodePattern::anonymous());
    let [PatternStep::Group { node, .. }] = part.element.steps.as_slice() else {
        panic!("expected one group, got {:?}", part.element.steps);
    };
    assert_eq!(*node, NodePattern::anonymous());
}

#[test]
fn malformed_quantifiers_are_rejected() {
    for (source, kind) in [
        (
            "MATCH (a)-->{3,1}(b) RETURN a",
            SyntaxErrorKind::InvalidRelationshipPattern,
        ),
        (
            "MATCH (a)-->{}(b) RETURN a",
            SyntaxErrorKind::InvalidRelationshipPattern,
        ),
        (
            "MATCH (a)-[*2]->{1,3}(b) RETURN a",
            SyntaxErrorKind::InvalidRelationshipPattern,
        ),
        (
            "MATCH (a)((x)-->(y))(b) RETURN a",
            SyntaxErrorKind::UnsupportedFeature,
        ),
        (
            "MATCH (a)((x)((u)-->(v))+(y))+(b) RETURN a",
            SyntaxErrorKind::UnsupportedFeature,
        ),
        (
            "MATCH SHORTEST (a)-->(b) RETURN a",
            SyntaxErrorKind::UnexpectedToken,
        ),
    ] {
        assert_eq!(error(source).1, kind, "{source}");
    }
}

#[test]
fn length_bounds_need_the_star_and_cannot_be_negative() {
    for (source, offset) in [
        ("MATCH (a)-[:T..]->(b) RETURN a", 13),
        ("MATCH (a)-[2]->(b) RETURN a", 11),
        ("MATCH (a)-[*-2]->(b) RETURN a", 12),
        ("MATCH (a)-[*1..-2]->(b) RETURN a", 15),
    ] {
        assert_eq!(
            error(source),
            (offset, SyntaxErrorKind::InvalidRelationshipPattern),
            "{source}"
        );
    }
}

#[test]
fn malformed_undirected_edges_report_offsets() {
    for (source, offset) in [
        ("MATCH (a)<~>(b) RETURN a", 11),
        ("MATCH (a)<~[]~>(b) RETURN a", 14),
        ("MATCH (a)~[r]-(b) RETURN a", 13),
        ("MATCH (a)-[r]~(b) RETURN a", 13),
        ("MATCH (a)~~(b) RETURN a", 10),
        ("MATCH (a)~-(b) RETURN a", 10),
    ] {
        assert_eq!(
            error(source),
            (offset, SyntaxErrorKind::UnexpectedToken),
            "{source}"
        );
    }
}

#[test]
fn an_undirected_edge_after_a_parenthesis_starts_a_pattern_predicate() {
    for source in ["(a)~(b)", "(a)<~(b)", "(a)~>(b)", "(a)~[:T]~(b)"] {
        assert!(matches!(predicate(source), Expr::Pattern(_)), "{source}");
    }
}

#[test]
fn malformed_patterns_report_offsets() {
    for (source, expected) in [
        ("MATCH (a RETURN a", (9, SyntaxErrorKind::UnexpectedToken)),
        (
            "MATCH (a)-[r->(b) RETURN a",
            (12, SyntaxErrorKind::UnexpectedToken),
        ),
        (
            "MATCH (a)-[*1..x]->(b) RETURN a",
            (15, SyntaxErrorKind::UnexpectedToken),
        ),
        (
            "MATCH (a)-[r]>(b) RETURN a",
            (13, SyntaxErrorKind::UnexpectedToken),
        ),
        ("MATCH (a)-->", (12, SyntaxErrorKind::UnexpectedEnd)),
        ("MATCH", (5, SyntaxErrorKind::UnexpectedEnd)),
        (
            "MATCH (a {k 1}) RETURN a",
            (12, SyntaxErrorKind::UnexpectedToken),
        ),
    ] {
        assert_eq!(error(source), expected, "{source}");
    }
}
