use std::collections::BTreeMap;

use super::*;
use crate::fake::Answer;
use crate::fake::Event;
use crate::fake::FakeEngine;
use crate::scenario::Control;
use crate::scenario::Expected;
use crate::scenario::GraphSetup;
use crate::scenario::Ordering;
use crate::scenario::Procedure;
use crate::side_effects::Counts;
use crate::side_effects::GraphSnapshot;
use crate::side_effects::SideEffects;
use crate::value::Lists;
use crate::value::Value;

fn scenario() -> Scenario {
    Scenario {
        feature: "f.feature".into(),
        name: "s".into(),
        example: None,
        tags: Default::default(),
        graph: GraphSetup::Empty,
        procedures: vec![],
        setup: vec![],
        parameters: BTreeMap::new(),
        query: "RETURN 1 AS n".into(),
        expected: one_row(1),
        side_effects: None,
        control: None,
    }
}

fn one_row(value: i64) -> Expected {
    Expected::Rows {
        columns: vec!["n".into()],
        rows: vec![vec![Value::Integer(value)]],
        ordering: Ordering::AnyOrder,
        lists: Lists::Strict,
    }
}

fn answer(query: &str, value: i64) -> Answer {
    Answer::rows(query, &["n"], vec![vec![Value::Integer(value)]])
}

fn with_nodes(ids: &[u64]) -> GraphSnapshot {
    GraphSnapshot {
        nodes: ids.to_vec(),
        ..GraphSnapshot::default()
    }
}

fn nodes_added(count: u64) -> Option<SideEffects> {
    Some(SideEffects {
        added: Counts {
            nodes: count,
            ..Counts::default()
        },
        removed: Counts::default(),
    })
}

#[test]
fn passes_when_the_engine_returns_the_expected_rows() {
    let engine = FakeEngine::new(vec![answer("RETURN 1 AS n", 1)]);

    assert_eq!(run_scenario(&engine, &scenario(), None), Ok(()));
}

#[test]
fn fails_when_the_engine_returns_other_rows() {
    let engine = FakeEngine::new(vec![answer("RETURN 1 AS n", 2)]);

    assert!(run_scenario(&engine, &scenario(), None).is_err());
}

#[test]
fn fails_when_the_engine_does_not_support_the_query() {
    let engine = FakeEngine::new(vec![]);

    let message = run_scenario(&engine, &scenario(), None).expect_err("unsupported");

    assert!(message.contains("does not support"), "{message}");
}

#[test]
fn the_graph_script_and_setup_run_before_the_query_without_parameters() {
    let engine = FakeEngine::new(vec![
        Answer::rows("CREATE script", &[], vec![]),
        Answer::rows("CREATE setup", &[], vec![]),
        answer("RETURN $p AS n", 1),
    ]);
    let scenario = Scenario {
        graph: GraphSetup::Named("g".into()),
        setup: vec!["CREATE setup".into()],
        parameters: BTreeMap::from([("p".to_string(), Value::Integer(1))]),
        query: "RETURN $p AS n".into(),
        ..scenario()
    };

    assert_eq!(
        run_scenario(&engine, &scenario, Some("CREATE script")),
        Ok(())
    );

    assert_eq!(
        engine.events(),
        vec![
            Event::Executed("CREATE script".into(), BTreeMap::new()),
            Event::Executed("CREATE setup".into(), BTreeMap::new()),
            Event::Executed(
                "RETURN $p AS n".into(),
                BTreeMap::from([("p".to_string(), Value::Integer(1))])
            ),
        ]
    );
}

#[test]
fn a_failing_setup_query_fails_the_scenario() {
    let engine = FakeEngine::new(vec![Answer::raised("CREATE setup", "SyntaxError", "x")]);
    let scenario = Scenario {
        setup: vec!["CREATE setup".into()],
        ..scenario()
    };

    let message = run_scenario(&engine, &scenario, None).expect_err("setup failure");

    assert!(message.contains("setup"), "{message}");
}

#[test]
fn procedures_are_registered_before_any_query_runs() {
    let engine = FakeEngine::new(vec![answer("RETURN 1 AS n", 1)]);
    let scenario = Scenario {
        procedures: vec![Procedure {
            signature: "test.proc()".into(),
            columns: vec![],
            rows: vec![],
        }],
        ..scenario()
    };

    assert_eq!(run_scenario(&engine, &scenario, None), Ok(()));

    assert_eq!(engine.events()[0], Event::Registered("test.proc()".into()));
}

#[test]
fn side_effects_are_measured_from_after_the_setup() {
    let engine = FakeEngine::new(vec![
        Answer::rows("CREATE setup", &[], vec![]).then_graph(with_nodes(&[1])),
        answer("RETURN 1 AS n", 1).then_graph(with_nodes(&[1, 2])),
    ]);
    let scenario = Scenario {
        setup: vec!["CREATE setup".into()],
        side_effects: nodes_added(1),
        ..scenario()
    };

    assert_eq!(run_scenario(&engine, &scenario, None), Ok(()));
}

#[test]
fn fails_when_the_side_effects_differ_from_the_expected_ones() {
    let engine = FakeEngine::new(vec![
        answer("RETURN 1 AS n", 1).then_graph(with_nodes(&[1, 2])),
    ]);

    let wrong_count = Scenario {
        side_effects: nodes_added(1),
        ..scenario()
    };
    let none_expected = Scenario {
        side_effects: Some(SideEffects::default()),
        ..scenario()
    };

    assert!(run_scenario(&engine, &wrong_count, None).is_err());
    assert!(run_scenario(&engine, &none_expected, None).is_err());
}

#[test]
fn side_effects_are_ignored_when_the_scenario_does_not_assert_them() {
    let engine = FakeEngine::new(vec![
        answer("RETURN 1 AS n", 1).then_graph(with_nodes(&[1, 2])),
    ]);

    assert_eq!(run_scenario(&engine, &scenario(), None), Ok(()));
}

#[test]
fn the_control_query_is_checked_after_the_main_query() {
    let control = Control {
        query: "MATCH (n) RETURN n".into(),
        expected: one_row(5),
    };
    let scenario = Scenario {
        control: Some(control),
        ..scenario()
    };
    let good = FakeEngine::new(vec![
        answer("RETURN 1 AS n", 1),
        answer("MATCH (n) RETURN n", 5),
    ]);
    let bad = FakeEngine::new(vec![
        answer("RETURN 1 AS n", 1),
        answer("MATCH (n) RETURN n", 6),
    ]);

    assert_eq!(run_scenario(&good, &scenario, None), Ok(()));
    let message = run_scenario(&bad, &scenario, None).expect_err("control mismatch");
    assert!(message.contains("control query"), "{message}");
}

#[test]
fn a_graph_that_cannot_be_read_fails_a_scenario_that_asserts_side_effects() {
    let before_unreadable = FakeEngine::new(vec![
        Answer::rows("CREATE setup", &[], vec![]).then_unreadable_graph("MATCH (n) failed"),
        answer("RETURN 1 AS n", 1),
    ]);
    let after_unreadable = FakeEngine::new(vec![
        answer("RETURN 1 AS n", 1).then_unreadable_graph("MATCH (n) failed"),
    ]);
    let setup = Scenario {
        setup: vec!["CREATE setup".into()],
        side_effects: nodes_added(0),
        ..scenario()
    };
    let plain = Scenario {
        side_effects: nodes_added(0),
        ..scenario()
    };

    let message = run_scenario(&before_unreadable, &setup, None).expect_err("before");
    assert!(
        message.contains("before the query") && message.contains("MATCH (n) failed"),
        "{message}"
    );
    let message = run_scenario(&after_unreadable, &plain, None).expect_err("after");
    assert!(
        message.contains("after the query") && message.contains("MATCH (n) failed"),
        "{message}"
    );
}

#[test]
fn an_unreadable_graph_does_not_matter_when_side_effects_are_not_asserted() {
    let engine = FakeEngine::new(vec![
        answer("RETURN 1 AS n", 1).then_unreadable_graph("MATCH (n) failed"),
    ]);

    assert_eq!(run_scenario(&engine, &scenario(), None), Ok(()));
}
