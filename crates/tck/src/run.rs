use std::collections::BTreeMap;

use crate::engine::Engine;
use crate::matching::check_outcome;
use crate::scenario::Scenario;

/// Runs one scenario on a fresh graph of `engine`. `graph_script` is the Cypher that builds a
/// named graph, if the scenario uses one. The error says how the engine deviated from the TCK.
pub fn run_scenario(
    engine: &dyn Engine,
    scenario: &Scenario,
    graph_script: Option<&str>,
) -> Result<(), String> {
    let mut graph = engine.fresh_graph();
    for procedure in &scenario.procedures {
        graph.register_procedure(procedure);
    }
    let no_parameters = BTreeMap::new();
    for query in graph_script
        .into_iter()
        .chain(scenario.setup.iter().map(String::as_str))
    {
        graph
            .execute(query, &no_parameters)
            .map_err(|error| format!("setup query failed with {error}: {query}"))?;
    }
    let before = scenario
        .side_effects
        .map(|_| graph.snapshot())
        .transpose()
        .map_err(|reason| format!("could not read the graph before the query: {reason}"))?;
    let result = graph.execute(&scenario.query, &scenario.parameters);
    check_outcome(&scenario.expected, &result)?;
    if let (Some(expected), Some(before)) = (scenario.side_effects, before) {
        let after = graph
            .snapshot()
            .map_err(|reason| format!("could not read the graph after the query: {reason}"))?;
        let actual = before.diff(&after);
        if actual != expected {
            return Err(format!(
                "expected side effects {expected:?}, got {actual:?}"
            ));
        }
    }
    if let Some(control) = &scenario.control {
        let result = graph.execute(&control.query, &scenario.parameters);
        check_outcome(&control.expected, &result)
            .map_err(|message| format!("control query: {message}"))?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "run.test.rs"]
mod tests;
