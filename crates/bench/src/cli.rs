use std::env;
use std::fmt;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use serde_json::json;

use crate::data::Dataset;
use crate::engine::Engine;
use crate::ladybug::LadybugEngine;
use crate::report::WorkloadReport;
use crate::report::json;
use crate::report::table;
use crate::rhizo::RhizoEngine;
use crate::runner::Environment;
use crate::runner::Settings;
use crate::runner::run_all;
use crate::scale::Scale;
use crate::workloads;

/// Seed of the generated dataset. Changing it changes every number.
pub const SEED: u64 = 20_261_122;

/// The command line of the benchmark binary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Arguments {
    pub scale: Scale,
    pub reps: usize,
    pub only: Option<String>,
    pub out: Option<PathBuf>,
}

/// A command line the binary cannot read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsageError(pub String);

impl fmt::Display for UsageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}\nusage: bench [--scale tiny|small|medium|large] [--reps N] [--only SUBSTRING] [--out FILE.json]",
            self.0
        )
    }
}

impl std::error::Error for UsageError {}

/// Reads `--scale`, `--reps`, `--only`, and `--out`. The scale defaults to `small` and the
/// repetitions to 5.
pub fn parse(arguments: impl IntoIterator<Item = String>) -> Result<Arguments, UsageError> {
    let mut parsed = Arguments {
        scale: Scale::Small,
        reps: 5,
        only: None,
        out: None,
    };
    let mut arguments = arguments.into_iter();
    while let Some(flag) = arguments.next() {
        let mut value = || {
            arguments
                .next()
                .ok_or_else(|| UsageError(format!("{flag} needs a value")))
        };
        match flag.as_str() {
            "--scale" => {
                let text = value()?;
                parsed.scale = Scale::parse(&text)
                    .ok_or_else(|| UsageError(format!("unknown scale `{text}`")))?;
            }
            "--reps" => {
                let text = value()?;
                parsed.reps = text
                    .parse()
                    .ok()
                    .filter(|reps| *reps > 0)
                    .ok_or_else(|| UsageError(format!("`{text}` is not a positive count")))?;
            }
            "--only" => parsed.only = Some(value()?),
            "--out" => parsed.out = Some(PathBuf::from(value()?)),
            other => return Err(UsageError(format!("unknown argument `{other}`"))),
        }
    }
    Ok(parsed)
}

/// Runs the suite on both engines and returns one report per workload.
pub fn benchmark(
    scale: Scale,
    reps: usize,
    only: Option<String>,
    progress: impl FnMut(&str),
) -> Result<Vec<WorkloadReport>, String> {
    let plan = scale.plan();
    let data = Dataset::generate(SEED, plan);
    let mut environment = Environment::new(&env::temp_dir(), &plan, &data)
        .map_err(|error| format!("cannot create the scratch directory: {error}"))?;
    let rhizo = RhizoEngine;
    let ladybug = LadybugEngine;
    let engines: [&dyn Engine; 2] = [&rhizo, &ladybug];
    let settings = Settings {
        reps,
        only,
        seed: SEED,
    };
    Ok(run_all(
        &mut environment,
        &engines,
        &workloads::all(),
        &settings,
        progress,
    ))
}

/// Parses the command line, runs the benchmark, prints the table, and writes the JSON.
pub fn main() -> ExitCode {
    let arguments = match parse(env::args().skip(1)) {
        Ok(arguments) => arguments,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(2);
        }
    };
    let reports = match benchmark(
        arguments.scale,
        arguments.reps,
        arguments.only.clone(),
        |name| eprintln!("running {name}"),
    ) {
        Ok(reports) => reports,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    let plan = arguments.scale.plan();
    print!("{}", table(&reports));
    if let Some(path) = &arguments.out {
        let header = json!({
            "scale": arguments.scale.name(),
            "reps": arguments.reps,
            "seed": SEED,
            "persons": plan.persons,
            "knows": plan.persons * plan.knows_per_person,
            "cities": plan.cities,
            "cores": std::thread::available_parallelism().map_or(0, |cores| cores.get()),
            "ladybugdb": "lbug 0.22.3",
        });
        let document = json(&reports, header);
        let text = serde_json::to_string_pretty(&document).unwrap_or_default();
        if let Err(error) = fs::write(path, text) {
            eprintln!("cannot write {}: {error}", path.display());
            return ExitCode::FAILURE;
        }
    }
    let broken = reports.iter().any(|report| {
        !report.disagreements().is_empty()
            || report.series.iter().any(|series| {
                series.failure.is_some() || series.runs.iter().any(|run| run.errors > 0)
            })
    });
    if broken {
        eprintln!("some workloads failed, errored, or disagreed between the engines");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
#[path = "cli.test.rs"]
mod tests;
