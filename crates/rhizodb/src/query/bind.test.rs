use crate::gql::parser::parse;
use crate::result::Parameters;

use super::super::plan::Plan;
use super::bind;

fn closed_count(query: &str) -> usize {
    match bind(&parse(query).unwrap(), query, &Parameters::new()).unwrap() {
        Plan::ReadOnly(program) => program.closed.len(),
        Plan::Updating(program) => program.closed.len(),
    }
}

#[test]
fn a_closed_expression_is_recorded_once_not_once_per_nested_operator() {
    assert_eq!(closed_count("RETURN 1 + 1 + 1 + 1 + 1"), 1);
}

#[test]
fn only_the_closed_parts_of_an_open_expression_are_recorded() {
    assert_eq!(closed_count("MATCH (n) RETURN n.x + (1 + 2) + (3 + 4)"), 2);
}

#[test]
fn closed_parts_that_conditional_evaluation_may_skip_are_not_recorded() {
    assert_eq!(
        closed_count("MATCH (n) RETURN CASE WHEN n.x THEN 1 + 2 ELSE 3 + 4 END"),
        0
    );
}
