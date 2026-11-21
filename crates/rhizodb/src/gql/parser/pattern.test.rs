use crate::gql::ast::Direction;
use crate::gql::ast::Expr;
use crate::gql::ast::Length;
use crate::gql::ast::Literal;
use crate::gql::ast::Name;
use crate::gql::ast::NodePattern;
use crate::gql::ast::PatternPart;
use crate::gql::ast::Properties;
use crate::gql::ast::RelationshipPattern;
use crate::gql::error::SyntaxErrorKind;
use crate::gql::testing::error;
use crate::gql::testing::patterns;

fn name(text: &str) -> Name {
    Name::new(text)
}

fn node(variable: Option<&str>, labels: &[&str]) -> NodePattern {
    NodePattern {
        variable: variable.map(name),
        labels: labels.iter().map(|label| name(label)).collect(),
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
    part.element.steps.remove(0).relationship
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
        ("(a)--(b)", Direction::Undirected),
        ("(a)<-->(b)", Direction::Undirected),
        ("(a)-[]->(b)", Direction::Outgoing),
        ("(a)<-[]-(b)", Direction::Incoming),
        ("(a)-[]-(b)", Direction::Undirected),
        ("(a)<-[]->(b)", Direction::Undirected),
        ("(a)->(b)", Direction::Outgoing),
        ("(a)<-(b)", Direction::Incoming),
    ] {
        assert_eq!(only_relationship(pattern).direction, direction, "{pattern}");
    }
}

#[test]
fn relationship_detail_variable_types_and_properties() {
    let found = only_relationship("(a)-[r:KNOWS|LIKES|:LOVES {since: 1}]->(b)");
    assert_eq!(found.variable, Some(name("r")));
    assert_eq!(
        found.types,
        vec![name("KNOWS"), name("LIKES"), name("LOVES")]
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
    assert_eq!(found.types, vec![name("T")]);
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
        .map(|s| s.relationship.direction)
        .collect();
    assert_eq!(
        directions,
        vec![
            Direction::Outgoing,
            Direction::Incoming,
            Direction::Undirected
        ]
    );
    let labels: Vec<Vec<Name>> = part
        .element
        .steps
        .iter()
        .map(|s| s.node.labels.clone())
        .collect();
    assert_eq!(labels, vec![vec![name("B")], vec![], vec![]]);
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
fn shortest_path_is_reported_as_unsupported() {
    assert_eq!(
        error("MATCH p = shortestPath((a)-[*]-(b)) RETURN p"),
        (10, SyntaxErrorKind::UnsupportedFeature)
    );
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
