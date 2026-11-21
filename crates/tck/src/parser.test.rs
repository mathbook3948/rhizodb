use super::*;

const SAMPLE: &str = "\
# a comment
#encoding: utf-8
@tagged
Feature: Sample

  free text describing the feature

  Background:
    Given an empty graph

  @a @b
  Scenario: [1] First
    When executing query:
      \"\"\"
      MATCH (n)
        RETURN n
      \"\"\"
    Then the result should be, in any order:
      | n    | m        |
      | 'a\\|b' | 'c\\\\d' |
    And no side effects

  Scenario Outline: [2] Outline
    When executing query: <q>
    Examples:
      | q |
      | 1 |
";

#[test]
fn parses_tags_background_scenarios_and_outlines() {
    let file = parse_feature(SAMPLE).expect("valid feature");

    assert_eq!(file.tags, vec!["@tagged"]);
    assert_eq!(file.background.len(), 1);
    assert_eq!(file.background[0].text, "an empty graph");
    assert_eq!(file.scenarios.len(), 2);
    assert_eq!(file.scenarios[0].name, "[1] First");
    assert_eq!(file.scenarios[0].tags, vec!["@a", "@b"]);
    assert!(!file.scenarios[0].outline);
    assert!(file.scenarios[1].outline);
    assert_eq!(
        file.scenarios[1].examples[0].rows,
        vec![vec!["q".to_string()], vec!["1".to_string()]]
    );
}

#[test]
fn doc_strings_lose_the_indent_of_their_delimiter_and_keep_deeper_indent() {
    let file = parse_feature(SAMPLE).expect("valid feature");

    assert_eq!(
        file.scenarios[0].steps[0].docstring.as_deref(),
        Some("MATCH (n)\n  RETURN n")
    );
}

#[test]
fn table_cells_are_trimmed_and_unescaped() {
    let file = parse_feature(SAMPLE).expect("valid feature");

    assert_eq!(
        file.scenarios[0].steps[1].table,
        Some(vec![
            vec!["n".to_string(), "m".to_string()],
            vec!["'a|b'".to_string(), "'c\\d'".to_string()],
        ])
    );
}

#[test]
fn a_backslash_before_another_character_stays_in_the_cell() {
    let file = parse_feature(
        "Feature: F\n  Scenario: S\n    Then the result should be empty\n      | '\\'' |\n",
    )
    .expect("valid feature");

    assert_eq!(
        file.scenarios[0].steps[0].table,
        Some(vec![vec!["'\\''".to_string()]])
    );
}

#[test]
fn inline_step_text_is_kept_whole() {
    let file = parse_feature(SAMPLE).expect("valid feature");

    assert_eq!(file.scenarios[1].steps[0].text, "executing query: <q>");
    assert_eq!(file.scenarios[1].steps[0].line, 24);
}

#[test]
fn text_inside_a_scenario_that_is_not_a_step_is_a_syntax_error() {
    let error =
        parse_feature("Feature: F\n  Scenario: S\n    Given an empty graph\n    nonsense\n")
            .expect_err("syntax error");

    assert_eq!(error.line, 4);
}

#[test]
fn an_unclosed_doc_string_is_a_syntax_error() {
    assert!(
        parse_feature(
            "Feature: F\n  Scenario: S\n    When executing query:\n      \"\"\"\n      x\n"
        )
        .is_err()
    );
}

#[test]
fn examples_outside_an_outline_are_a_syntax_error() {
    assert!(
        parse_feature(
            "Feature: F\n  Scenario: S\n    Given any graph\n    Examples:\n      | a |\n"
        )
        .is_err()
    );
}

#[test]
fn carriage_returns_do_not_leak_into_values() {
    let file = parse_feature("Feature: F\r\n  Scenario: S\r\n    Given any graph\r\n    And parameters are:\r\n      | a | 1 |\r\n")
        .expect("valid feature");

    assert_eq!(file.scenarios[0].name, "S");
    assert_eq!(file.scenarios[0].steps[0].text, "any graph");
    assert_eq!(
        file.scenarios[0].steps[1].table,
        Some(vec![vec!["a".to_string(), "1".to_string()]])
    );
}
