use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

use crate::engine::Engine;
use crate::error::HarnessError;
use crate::feature::Case;
use crate::feature::compile_feature;
use crate::run::run_scenario;
use crate::scenario::GraphSetup;

/// How one scenario fared.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The engine did what the scenario expects.
    Passed,
    /// The engine deviated from the scenario.
    Failed(String),
    /// The harness could not compile or run the scenario.
    HarnessError(String),
}

/// Compiles every feature file under `vendor/features` into cases, ordered by path.
/// Cases whose trial name repeats an earlier one become harness errors.
pub fn load_cases(vendor: &Path) -> Result<Vec<Case>, HarnessError> {
    let features = vendor.join("features");
    let mut files = Vec::new();
    collect_features(&features, &mut files).map_err(|error| HarnessError {
        message: format!("cannot read {}: {error}", features.display()),
    })?;
    files.sort();
    let mut cases = Vec::new();
    for file in files {
        let relative = file
            .strip_prefix(&features)
            .unwrap_or(&file)
            .to_string_lossy()
            .replace('\\', "/");
        match fs::read_to_string(&file) {
            Ok(text) => cases.extend(compile_feature(&relative, &text)),
            Err(error) => cases.push(Case {
                name: relative.clone(),
                tags: BTreeSet::new(),
                scenario: Err(HarnessError {
                    message: format!("{relative}: {error}"),
                }),
            }),
        }
    }
    let mut seen = BTreeSet::new();
    for case in &mut cases {
        if !seen.insert(case.name.clone()) {
            case.scenario = Err(HarnessError {
                message: format!("duplicate trial name {}", case.name),
            });
        }
    }
    Ok(cases)
}

fn collect_features(dir: &Path, files: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_features(&path, files)?;
        } else if path.extension().is_some_and(|ext| ext == "feature") {
            files.push(path);
        }
    }
    Ok(())
}

/// Runs one case against `engine`. Named graphs are read from `vendor/graphs`.
pub fn evaluate(engine: &dyn Engine, case: &Case, vendor: &Path) -> Verdict {
    let scenario = match &case.scenario {
        Ok(scenario) => scenario,
        Err(error) => return Verdict::HarnessError(error.message.clone()),
    };
    let script = match &scenario.graph {
        GraphSetup::Named(name) => {
            let path = vendor
                .join("graphs")
                .join(name)
                .join(format!("{name}.cypher"));
            match fs::read_to_string(&path) {
                Ok(script) => Some(script),
                Err(error) => {
                    return Verdict::HarnessError(format!(
                        "cannot read {}: {error}",
                        path.display()
                    ));
                }
            }
        }
        GraphSetup::Empty | GraphSetup::Any => None,
    };
    match run_scenario(engine, scenario, script.as_deref()) {
        Ok(()) => Verdict::Passed,
        Err(message) => Verdict::Failed(message),
    }
}

#[cfg(test)]
#[path = "suite.test.rs"]
mod tests;
