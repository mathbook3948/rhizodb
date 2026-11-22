use crate::gql::parser::parse;
use crate::result::Parameters;

use super::super::plan::Clause;
use super::super::plan::Plan;
use super::super::plan::UpdateClause;
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
fn the_closed_parts_of_an_aggregate_argument_are_recorded() {
    assert_eq!(closed_count("MATCH (n) RETURN count(n.x + (1 + 2))"), 1);
}

#[test]
fn the_closed_parts_inside_a_pattern_are_recorded_but_not_the_pattern() {
    assert_eq!(
        closed_count("MATCH (n) WHERE (n)-->({x: 1 + 2}) RETURN n"),
        1
    );
    assert_eq!(
        closed_count("MATCH (n) RETURN [(n)-->(m) | m.x + (1 + 2)]"),
        1
    );
}

fn delete_retains(query: &str) -> Vec<bool> {
    let Plan::Updating(program) = bind(&parse(query).unwrap(), query, &Parameters::new()).unwrap()
    else {
        panic!("{query} is not an updating query");
    };
    program
        .parts
        .iter()
        .flat_map(|part| &part.clauses)
        .filter_map(|clause| match clause {
            Clause::Update(UpdateClause::Delete { retain, .. }) => Some(*retain),
            _ => None,
        })
        .collect()
}

#[test]
fn a_delete_whose_result_cannot_hold_an_element_retains_no_records() {
    assert_eq!(delete_retains("MATCH (n) DETACH DELETE n"), [false]);
    assert_eq!(
        delete_retains("MATCH (n) DELETE n RETURN count(*) AS c"),
        [false]
    );
    assert_eq!(
        delete_retains("MATCH (n) DELETE n RETURN 1 AS one"),
        [false]
    );
}

#[test]
fn a_delete_whose_result_can_hold_an_element_retains_records() {
    assert_eq!(delete_retains("MATCH (n) DELETE n RETURN n"), [true]);
    assert_eq!(
        delete_retains("MATCH (n) DELETE n WITH n AS m RETURN m"),
        [true]
    );
    assert_eq!(
        delete_retains("MATCH (n) DELETE n RETURN count(n) AS c"),
        [true]
    );
}

#[test]
fn closed_parts_that_conditional_evaluation_may_skip_are_still_recorded() {
    assert_eq!(
        closed_count("MATCH (n) RETURN CASE WHEN n.x THEN 1 + 2 ELSE 3 + 4 END"),
        2
    );
}
