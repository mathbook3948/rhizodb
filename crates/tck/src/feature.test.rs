use super::*;
use crate::scenario::Expected;
use crate::value::Value;

const OUTLINE: &str = "\
Feature: Outlines

  Background:
    Given an empty graph

  @outline
  Scenario Outline: [1] Echo
    When executing query:
      \"\"\"
      RETURN <value> AS <alias>, 1 < 2 AS cmp
      \"\"\"
    Then the result should be, in any order:
      | <alias> | cmp  |
      | <value> | true |
    And no side effects

    @first
    Examples:
      | value | alias |
      | 1     | one   |
      | 'x'   | other |

    @ignore
    Examples:
      | value | alias |
      | 3     | three |

  Scenario: [2] Plain
    When executing query:
      \"\"\"
      RETURN 1 AS n
      \"\"\"
    Then the result should be empty
";

fn scenarios(cases: &[Case]) -> Vec<&Scenario> {
    cases
        .iter()
        .map(|case| case.scenario.as_ref().expect("compiles"))
        .collect()
}

#[test]
fn an_outline_becomes_one_case_per_example_row_across_all_tables() {
    let cases = compile_feature("o.feature", OUTLINE);

    let names: Vec<&str> = cases.iter().map(|case| case.name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "o.feature: [1] Echo #1",
            "o.feature: [1] Echo #2",
            "o.feature: [1] Echo #3",
            "o.feature: [2] Plain",
        ]
    );
}

#[test]
fn placeholders_are_replaced_in_docstrings_and_table_cells() {
    let cases = compile_feature("o.feature", OUTLINE);

    let second = scenarios(&cases)[1];

    assert_eq!(second.query, "RETURN 'x' AS other, 1 < 2 AS cmp");
    assert_eq!(
        second.expected,
        Expected::Rows {
            columns: vec!["other".into(), "cmp".into()],
            rows: vec![vec![Value::String("x".into()), Value::Boolean(true)]],
            ordering: crate::scenario::Ordering::AnyOrder,
            lists: crate::value::Lists::Strict,
        }
    );
}

#[test]
fn placeholders_are_replaced_in_step_text() {
    let text = "\
Feature: F
  Scenario Outline: [1] Named
    Given the <graph> graph
    When executing query:
      \"\"\"
      RETURN 1 AS n
      \"\"\"
    Then the result should be empty
    Examples:
      | graph         |
      | binary-tree-2 |
";

    let cases = compile_feature("f.feature", text);

    assert_eq!(
        scenarios(&cases)[0].graph,
        crate::scenario::GraphSetup::Named("binary-tree-2".into())
    );
}

#[test]
fn text_in_angle_brackets_that_is_not_a_column_is_left_alone() {
    let cases = compile_feature("o.feature", OUTLINE);

    assert!(scenarios(&cases)[0].query.ends_with("1 < 2 AS cmp"));
}

#[test]
fn tags_of_the_feature_scenario_and_example_table_are_merged() {
    let cases = compile_feature("o.feature", OUTLINE);

    let tags =
        |index: usize| -> Vec<&str> { cases[index].tags.iter().map(String::as_str).collect() };
    assert_eq!(tags(0), vec!["first", "outline"]);
    assert_eq!(tags(2), vec!["ignore", "outline"]);
    assert!(tags(3).is_empty());
}

#[test]
fn background_steps_come_first_in_every_scenario() {
    let cases = compile_feature("o.feature", OUTLINE);

    for scenario in scenarios(&cases) {
        assert_eq!(scenario.graph, crate::scenario::GraphSetup::Empty);
    }
}

#[test]
fn a_plain_scenario_has_no_example_index() {
    let cases = compile_feature("o.feature", OUTLINE);

    let plain = scenarios(&cases)[3];

    assert_eq!(plain.example, None);
    assert_eq!(plain.side_effects, None);
    assert_eq!(plain.trial_name(), cases[3].name);
}

#[test]
fn an_unknown_step_is_a_harness_error_on_that_scenario_only() {
    let text = "\
Feature: F
  Scenario: [1] Bad
    Given an empty graph
    When the moon is full
  Scenario: [2] Good
    Given an empty graph
    When executing query:
      \"\"\"
      RETURN 1 AS n
      \"\"\"
    Then the result should be empty
";

    let cases = compile_feature("f.feature", text);

    let error = cases[0].scenario.as_ref().expect_err("harness error");
    assert!(error.message.contains("unrecognized step"), "{error}");
    assert!(error.message.contains("f.feature"), "{error}");
    assert!(cases[1].scenario.is_ok());
}

#[test]
fn an_unparsable_file_is_one_harness_error_named_after_the_file() {
    let cases = compile_feature(
        "f.feature",
        "Feature: F\n  Scenario: S\n    Given any graph\n    garbage\n",
    );

    assert_eq!(cases.len(), 1);
    assert_eq!(cases[0].name, "f.feature");
    assert!(cases[0].scenario.is_err());
}

#[test]
fn an_outline_without_examples_is_a_harness_error() {
    let text = "Feature: F\n  Scenario Outline: [1] Empty\n    Given an empty graph\n";

    let cases = compile_feature("f.feature", text);

    assert_eq!(cases.len(), 1);
    assert!(cases[0].scenario.is_err());
}
