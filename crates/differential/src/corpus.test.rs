use std::collections::BTreeMap;

use tck::scenario::Scenario;

use super::*;

fn case(name: &str, graph: GraphSetup, setup: &[&str], query: &str, expected: Expected) -> Case {
    Case {
        name: name.into(),
        tags: BTreeSet::new(),
        scenario: Ok(Scenario {
            feature: "f.feature".into(),
            name: name.into(),
            example: None,
            tags: BTreeSet::new(),
            graph,
            procedures: Vec::new(),
            setup: setup.iter().map(|query| query.to_string()).collect(),
            parameters: BTreeMap::new(),
            query: query.into(),
            expected,
            side_effects: None,
            control: None,
        }),
    }
}

fn rows(ordering: Ordering) -> Expected {
    Expected::Rows {
        columns: vec!["a".into()],
        rows: Vec::new(),
        ordering,
        lists: Lists::Strict,
    }
}

#[test]
fn only_passing_graph_free_tck_scenarios_that_expect_rows_are_taken() {
    let cases = vec![
        case(
            "kept",
            GraphSetup::Any,
            &[],
            "RETURN 1 AS a",
            rows(Ordering::InOrder),
        ),
        case(
            "failing",
            GraphSetup::Any,
            &[],
            "RETURN 1",
            rows(Ordering::AnyOrder),
        ),
        case(
            "setup",
            GraphSetup::Empty,
            &["CREATE ()"],
            "RETURN 1",
            rows(Ordering::AnyOrder),
        ),
        case(
            "named",
            GraphSetup::Named("binary-tree-1".into()),
            &[],
            "RETURN 1",
            rows(Ordering::AnyOrder),
        ),
        case(
            "reads",
            GraphSetup::Empty,
            &[],
            "OPTIONAL match (n) RETURN n",
            rows(Ordering::AnyOrder),
        ),
        case(
            "writes",
            GraphSetup::Empty,
            &[],
            "CREATE (n) RETURN n",
            rows(Ordering::AnyOrder),
        ),
        case("error", GraphSetup::Empty, &[], "RETURN x", Expected::Empty),
    ];
    let passing: BTreeSet<String> = ["kept", "setup", "named", "reads", "writes", "error"]
        .iter()
        .map(|name| name.to_string())
        .collect();

    let queries = from_tck(&cases, &passing);

    assert_eq!(
        queries,
        vec![Query {
            name: "tck: kept".into(),
            text: "RETURN 1 AS a".into(),
            fixture: Fixture::Empty,
            ordering: Ordering::InOrder,
            lists: Lists::Strict,
        }]
    );
}

#[test]
fn a_word_inside_another_word_does_not_count_as_a_clause() {
    assert!(!touches_the_graph("RETURN 'x' AS offset, 1 AS matches"));
    assert!(touches_the_graph("UNWIND [1] AS x MATCH (n) RETURN n"));
}

#[test]
fn hand_written_queries_skip_comments_and_order_by_their_last_return() {
    let queries = hand_written(
        "# a comment\n\nMATCH (p:Person) RETURN p.id ORDER BY p.id\nMATCH (p:Person) WITH p ORDER BY p.id RETURN p.id\n",
    );

    assert_eq!(queries.len(), 2);
    assert_eq!(queries[0].ordering, Ordering::InOrder);
    assert_eq!(queries[1].ordering, Ordering::AnyOrder);
    assert_eq!(queries[0].name, format!("corpus: {}", queries[0].text));
}

#[test]
fn hand_written_queries_that_collect_ignore_list_element_order() {
    let queries = hand_written("MATCH (p:Person) RETURN collect(p.id)\nRETURN [1, 2]\n");

    assert_eq!(queries[0].lists, Lists::IgnoreElementOrder);
    assert_eq!(queries[1].lists, Lists::Strict);
}

#[test]
fn generated_queries_are_reproducible_and_unique() {
    let first = generated(11, 200);

    assert_eq!(first, generated(11, 200));
    assert_eq!(first.len(), 200);
    let names: BTreeSet<&str> = first.iter().map(|query| query.name.as_str()).collect();
    assert_eq!(names.len(), first.len());
}

#[test]
fn a_generated_query_is_compared_in_order_only_when_it_orders_every_column() {
    for query in generated(11, 200) {
        let ordered = query.text.contains(" ORDER BY ");
        assert_eq!(
            query.ordering == Ordering::InOrder,
            ordered,
            "{}",
            query.text
        );
        if ordered {
            let width = query.text.matches(" AS ").count();
            let keys = query.text.split(" ORDER BY ").nth(1).unwrap_or_default();
            assert_eq!(keys.split(", ").count(), width, "{}", query.text);
        }
        let paged = query.text.contains(" SKIP ") || query.text.contains(" LIMIT ");
        assert!(!paged || ordered, "{}", query.text);
    }
}

#[test]
fn collected_lists_are_compared_without_element_order() {
    let queries = generated(11, 400);

    assert!(queries.iter().any(|query| query.text.contains("collect(")));
    for query in queries {
        let collects = query.text.contains("collect(");
        assert_eq!(
            query.lists == Lists::IgnoreElementOrder,
            collects,
            "{}",
            query.text
        );
    }
}
