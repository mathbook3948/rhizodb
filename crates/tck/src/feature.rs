use std::collections::BTreeMap;
use std::collections::BTreeSet;

use crate::error::HarnessError;
use crate::parser::ExamplesBlock;
use crate::parser::parse_feature;
use crate::scenario::Scenario;
use crate::scenario::trial_name;
use crate::steps::RawStep;
use crate::steps::compile_steps;

/// One schedulable unit: a scenario, or the reason it could not be compiled.
#[derive(Clone, Debug, PartialEq)]
pub struct Case {
    /// Stable, unique trial name.
    pub name: String,
    /// Tags without the leading `@`.
    pub tags: BTreeSet<String>,
    /// The compiled scenario, or the harness error that prevented it.
    pub scenario: Result<Scenario, HarnessError>,
}

/// Compiles every scenario of one feature file. Outlines become one case per example row.
pub fn compile_feature(path: &str, text: &str) -> Vec<Case> {
    let file = match parse_feature(text) {
        Ok(file) => file,
        Err(error) => return vec![failed_case(path.to_string(), format!("{path}: {error}"))],
    };
    let feature_tags = clean_tags(&file.tags);
    let mut cases = Vec::new();
    for block in &file.scenarios {
        let mut tags = feature_tags.clone();
        tags.extend(clean_tags(&block.tags));
        let steps = || file.background.iter().chain(&block.steps);
        if !block.outline {
            let steps = steps().cloned().collect();
            cases.push(case(path, &block.name, None, tags, steps));
            continue;
        }
        let examples = example_rows(&block.examples);
        if examples.is_empty() {
            let message = format!("{path}: {}: outline has no example rows", block.name);
            cases.push(failed_case(trial_name(path, &block.name, None), message));
        }
        for (index, (example_tags, bindings)) in examples.into_iter().enumerate() {
            let mut tags = tags.clone();
            tags.extend(example_tags);
            let steps = steps()
                .map(|step| substitute_step(step, &bindings))
                .collect();
            cases.push(case(path, &block.name, Some(index), tags, steps));
        }
    }
    cases
}

type Bindings = BTreeMap<String, String>;

fn example_rows(examples: &[ExamplesBlock]) -> Vec<(BTreeSet<String>, Bindings)> {
    let mut rows = Vec::new();
    for block in examples {
        let Some((header, body)) = block.rows.split_first() else {
            continue;
        };
        for row in body {
            let bindings = header.iter().cloned().zip(row.iter().cloned()).collect();
            rows.push((clean_tags(&block.tags), bindings));
        }
    }
    rows
}

fn clean_tags(tags: &[String]) -> BTreeSet<String> {
    tags.iter()
        .map(|tag| tag.trim_start_matches('@').to_string())
        .collect()
}

fn substitute_step(step: &RawStep, bindings: &Bindings) -> RawStep {
    RawStep {
        text: substitute(&step.text, bindings),
        docstring: step
            .docstring
            .as_ref()
            .map(|docstring| substitute(docstring, bindings)),
        table: step.table.as_ref().map(|table| {
            table
                .iter()
                .map(|row| row.iter().map(|cell| substitute(cell, bindings)).collect())
                .collect()
        }),
        line: step.line,
    }
}

fn substitute(text: &str, bindings: &Bindings) -> String {
    let mut output = String::new();
    let mut rest = text;
    while let Some(open) = rest.find('<') {
        output.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let bound = after
            .find('>')
            .and_then(|close| bindings.get(&after[..close]).map(|value| (close, value)));
        match bound {
            Some((close, value)) => {
                output.push_str(value);
                rest = &after[close + 1..];
            }
            None => {
                output.push('<');
                rest = after;
            }
        }
    }
    output.push_str(rest);
    output
}

fn case(
    path: &str,
    name: &str,
    example: Option<usize>,
    tags: BTreeSet<String>,
    steps: Vec<RawStep>,
) -> Case {
    let scenario = compile_steps(&steps)
        .map(|parts| Scenario {
            feature: path.to_string(),
            name: name.to_string(),
            example,
            tags: tags.clone(),
            graph: parts.graph,
            procedures: parts.procedures,
            setup: parts.setup,
            parameters: parts.parameters,
            query: parts.query,
            expected: parts.expected,
            side_effects: parts.side_effects,
            control: parts.control,
        })
        .map_err(|message| HarnessError {
            message: format!("{path}: {name}: {message}"),
        });
    Case {
        name: trial_name(path, name, example),
        tags,
        scenario,
    }
}

fn failed_case(name: String, message: String) -> Case {
    Case {
        name,
        tags: BTreeSet::new(),
        scenario: Err(HarnessError { message }),
    }
}

#[cfg(test)]
#[path = "feature.test.rs"]
mod tests;
