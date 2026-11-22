use std::collections::BTreeSet;

use super::*;
use crate::engine::Unimplemented;
use crate::fake::Answer;
use crate::fake::FakeEngine;
use crate::ratchet::Ratchet;
use crate::side_effects::GraphSnapshot;
use crate::value::Node;
use crate::value::Value;

fn vendor() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("vendor/opencypher")
}

fn find<'a>(cases: &'a [Case], name: &str) -> &'a Case {
    cases
        .iter()
        .find(|case| case.name == name)
        .unwrap_or_else(|| panic!("no case named {name}"))
}

fn node(labels: &[&str], name: Option<&str>) -> Value {
    Value::Node(Node {
        labels: labels.iter().map(|label| label.to_string()).collect(),
        properties: name
            .map(|name| ("name".to_string(), Value::String(name.into())))
            .into_iter()
            .collect(),
    })
}

const MATCH_ALL: &str = "clauses/match/Match1.feature: [2] Matching all nodes";
const CREATE_ONE: &str = "clauses/create/Create1.feature: [1] Create a single node";
const SETUP: &str = "CREATE (:A), (:B {name: 'b'}), ({name: 'c'})";
const MATCH_QUERY: &str = "MATCH (n)\nRETURN n";

fn engine_for_the_two_scenarios(name_of_b: &str) -> FakeEngine {
    let three_nodes = GraphSnapshot {
        nodes: vec![1, 2, 3],
        ..GraphSnapshot::default()
    };
    let one_node = GraphSnapshot {
        nodes: vec![1],
        ..GraphSnapshot::default()
    };
    FakeEngine::new(vec![
        Answer::rows(SETUP, &[], vec![]).then_graph(three_nodes),
        Answer::rows(
            MATCH_QUERY,
            &["n"],
            vec![
                vec![node(&["B"], Some(name_of_b))],
                vec![node(&[], Some("c"))],
                vec![node(&["A"], None)],
            ],
        ),
        Answer::rows("CREATE ()", &[], vec![]).then_graph(one_node),
    ])
}

#[test]
fn every_vendored_scenario_compiles() {
    let cases = load_cases(&vendor()).expect("vendor directory");

    let errors: Vec<String> = cases
        .iter()
        .filter_map(|case| case.scenario.as_ref().err())
        .map(|error| error.message.clone())
        .collect();
    assert!(
        errors.is_empty(),
        "{} harness errors, first: {:?}",
        errors.len(),
        errors.first()
    );
    assert!(cases.len() >= 1615, "found only {} cases", cases.len());
}

#[test]
fn trial_names_are_unique() {
    let cases = load_cases(&vendor()).expect("vendor directory");

    let names: BTreeSet<&str> = cases.iter().map(|case| case.name.as_str()).collect();
    assert_eq!(names.len(), cases.len());
}

#[test]
fn outline_rows_of_a_vendored_feature_get_their_own_trials() {
    let cases = load_cases(&vendor()).expect("vendor directory");

    let first = "clauses/match/Match1.feature: [7] Fail when a relationship has the same variable in a preceding MATCH #1";
    assert!(cases.iter().any(|case| case.name == first));
    assert!(
        cases
            .iter()
            .any(|case| case.name == first.replace("#1", "#2"))
    );
}

#[test]
fn only_the_ignored_vendored_scenario_is_tagged_ignore() {
    let cases = load_cases(&vendor()).expect("vendor directory");

    let ignored: Vec<&str> = cases
        .iter()
        .filter(|case| case.tags.contains("ignore"))
        .map(|case| case.name.as_str())
        .collect();
    assert_eq!(
        ignored,
        vec!["expressions/graph/Graph5.feature: [2] Single-labels expression on relationships"]
    );
}

#[test]
fn a_correct_engine_passes_real_scenarios_and_a_wrong_one_fails() {
    let cases = load_cases(&vendor()).expect("vendor directory");
    let correct = engine_for_the_two_scenarios("b");
    let perturbed = engine_for_the_two_scenarios("not b");

    assert_eq!(
        evaluate(&correct, find(&cases, MATCH_ALL), &vendor()),
        Verdict::Passed
    );
    assert_eq!(
        evaluate(&correct, find(&cases, CREATE_ONE), &vendor()),
        Verdict::Passed
    );
    assert!(matches!(
        evaluate(&perturbed, find(&cases, MATCH_ALL), &vendor()),
        Verdict::Failed(_)
    ));
}

#[test]
fn a_missing_side_effect_fails_a_real_scenario() {
    let cases = load_cases(&vendor()).expect("vendor directory");
    let creates_nothing = FakeEngine::new(vec![Answer::rows("CREATE ()", &[], vec![])]);

    assert!(matches!(
        evaluate(&creates_nothing, find(&cases, CREATE_ONE), &vendor()),
        Verdict::Failed(_)
    ));
}

#[test]
fn the_unimplemented_engine_fails_every_scenario_at_the_engine() {
    let cases = load_cases(&vendor()).expect("vendor directory");

    let passed = cases
        .iter()
        .filter(|case| evaluate(&Unimplemented, case, &vendor()) == Verdict::Passed)
        .count();

    assert_eq!(passed, 0);
}

#[test]
fn a_named_graph_script_is_read_from_the_vendor_directory_and_runs_first() {
    let cases = load_cases(&vendor()).expect("vendor directory");
    let named = cases
        .iter()
        .find(|case| {
            matches!(&case.scenario, Ok(s) if s.graph == crate::scenario::GraphSetup::Named("binary-tree-1".into()))
        })
        .expect("a scenario on binary-tree-1");
    let engine = FakeEngine::new(vec![]);

    assert!(matches!(
        evaluate(&engine, named, &vendor()),
        Verdict::Failed(_)
    ));

    let first = engine
        .events()
        .into_iter()
        .next()
        .expect("an executed query");
    let crate::fake::Event::Executed(query, _) = first else {
        panic!("expected an executed query");
    };
    assert!(query.starts_with("CREATE (a:A {name: 'a'}),"), "{query}");
}

#[test]
fn a_named_graph_with_a_missing_script_is_a_harness_error() {
    let cases = load_cases(&vendor()).expect("vendor directory");
    let named = cases
        .iter()
        .find(|case| matches!(&case.scenario, Ok(s) if s.graph != crate::scenario::GraphSetup::Empty && s.graph != crate::scenario::GraphSetup::Any))
        .expect("a named graph scenario");

    let verdict = evaluate(&Unimplemented, named, Path::new("/nonexistent"));

    assert!(matches!(verdict, Verdict::HarnessError(_)));
}

#[test]
fn the_four_ratchet_outcomes_hold_end_to_end() {
    let cases = load_cases(&vendor()).expect("vendor directory");
    let case = find(&cases, CREATE_ONE);
    let passing = engine_for_the_two_scenarios("b");
    let failing = FakeEngine::new(vec![]);
    let listed = Ratchet::parse(&format!("{CREATE_ONE}\n"));
    let unlisted = Ratchet::default();
    let judge = |ratchet: &Ratchet, engine: &FakeEngine| {
        ratchet.judge(&case.name, &evaluate(engine, case, &vendor()))
    };

    assert_eq!(judge(&listed, &passing), Ok(()));
    assert!(
        judge(&listed, &failing)
            .expect_err("regression")
            .contains("regression")
    );
    assert!(
        judge(&unlisted, &passing)
            .expect_err("unrecorded")
            .contains("TCK_BLESS=1")
    );
    assert_eq!(judge(&unlisted, &failing), Ok(()));
}

#[test]
fn a_harness_error_fails_the_ratchet_whether_listed_or_not() {
    let broken = Case {
        name: "x.feature: [1] broken".into(),
        tags: BTreeSet::new(),
        scenario: Err(HarnessError {
            message: "unrecognized step".into(),
        }),
    };
    let verdict = evaluate(&Unimplemented, &broken, &vendor());

    assert_eq!(verdict, Verdict::HarnessError("unrecognized step".into()));
    assert!(Ratchet::default().judge(&broken.name, &verdict).is_err());
    assert!(
        Ratchet::parse("x.feature: [1] broken\n")
            .judge(&broken.name, &verdict)
            .is_err()
    );
}

fn gql() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("gql")
}

#[test]
fn every_gql_scenario_compiles() {
    let cases = load_cases(&gql()).expect("gql directory");

    assert!(!cases.is_empty());
    for case in &cases {
        assert!(case.scenario.is_ok(), "{}: {:?}", case.name, case.scenario);
    }
}

#[test]
fn every_gql_feature_cites_the_iso_clause_its_directory_names() {
    let mut checked = 0;
    for dir in fs::read_dir(gql().join("features")).expect("gql features") {
        let dir = dir.expect("clause directory").path();
        let name = dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned());
        let name = name.unwrap_or_default();
        let (clause, rule) = name
            .split_once('-')
            .unwrap_or_else(|| panic!("{name} is not <clause>-<rule>"));
        assert!(
            clause.split('.').all(|part| part.parse::<u32>().is_ok()),
            "{name} does not start with an ISO/IEC 39075 clause number"
        );
        let citation = format!("ISO/IEC 39075:2024 {clause} <{}>", rule.replace('-', " "));
        for file in fs::read_dir(&dir).expect("clause directory") {
            let file = file.expect("feature file").path();
            let text = fs::read_to_string(&file).expect("feature text");
            assert!(
                text.lines()
                    .any(|line| line.starts_with("Feature:") && line.contains(&citation)),
                "{} does not cite {citation}",
                file.display()
            );
            checked += 1;
        }
    }
    assert!(checked > 0, "no gql features");
}
