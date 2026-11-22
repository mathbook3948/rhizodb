use std::env;
use std::fmt;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use serde_json::json;

use crate::bolt::BoltEngine;
use crate::bolt::Flavor;
use crate::data::Dataset;
use crate::engine::Engine;
use crate::engine::EngineKind;
use crate::falkor::FalkorEngine;
use crate::ladybug::LadybugEngine;
use crate::report::WorkloadReport;
use crate::report::json;
use crate::report::table;
use crate::report::unavailable;
use crate::rhizo::RhizoEngine;
use crate::runner::Environment;
use crate::runner::Settings;
use crate::runner::run_all;
use crate::scale::Scale;
use crate::sqlite::SqliteEngine;
use crate::workloads;

/// Seed of the generated dataset. Changing it changes every number.
pub const SEED: u64 = 20_261_122;

/// Fewer repetitions than this make the median and the range too thin to compare engines.
pub const MIN_REPS: usize = 5;

/// The servers to measure, each by the URL it listens on. A server without a URL is skipped.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Servers {
    pub neo4j: Option<String>,
    pub memgraph: Option<String>,
    pub falkordb: Option<String>,
}

/// The command line of the benchmark binary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Arguments {
    pub scale: Scale,
    pub reps: usize,
    pub only: Option<String>,
    pub skip: Option<String>,
    pub out: Option<PathBuf>,
    pub engines: Option<Vec<EngineKind>>,
    pub servers: Servers,
}

/// A command line the binary cannot read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsageError(pub String);

impl fmt::Display for UsageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}\nusage: bench [--scale tiny|small|medium|large] [--reps N] [--only SUBSTRING] [--skip SUBSTRING] [--engines a,b] [--out FILE.json] [--neo4j bolt://HOST:PORT] [--memgraph bolt://HOST:PORT] [--falkordb redis://HOST:PORT]",
            self.0
        )
    }
}

impl std::error::Error for UsageError {}

/// Reads `--scale`, `--reps`, `--only`, `--engines`, `--out`, and the server URLs. The scale
/// defaults to `small` and the repetitions to 5.
pub fn parse(arguments: impl IntoIterator<Item = String>) -> Result<Arguments, UsageError> {
    let mut parsed = Arguments {
        scale: Scale::Small,
        reps: MIN_REPS,
        only: None,
        skip: None,
        out: None,
        engines: None,
        servers: Servers::default(),
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
            "--skip" => parsed.skip = Some(value()?),
            "--out" => parsed.out = Some(PathBuf::from(value()?)),
            "--engines" => {
                let text = value()?;
                let engines: Option<Vec<EngineKind>> =
                    text.split(',').map(EngineKind::parse).collect();
                parsed.engines =
                    Some(engines.ok_or_else(|| UsageError(format!("unknown engine in `{text}`")))?);
            }
            "--neo4j" => parsed.servers.neo4j = Some(value()?),
            "--memgraph" => parsed.servers.memgraph = Some(value()?),
            "--falkordb" => parsed.servers.falkordb = Some(value()?),
            other => return Err(UsageError(format!("unknown argument `{other}`"))),
        }
    }
    Ok(parsed)
}

/// What a benchmark run needs to know.
#[derive(Clone, Debug)]
pub struct Options {
    pub scale: Scale,
    pub reps: usize,
    pub only: Option<String>,
    pub skip: Option<String>,
    pub engines: Option<Vec<EngineKind>>,
    pub servers: Servers,
}

/// What a benchmark run produced.
#[derive(Debug)]
pub struct Outcome {
    pub reports: Vec<WorkloadReport>,
    /// How each engine that ran was configured and which version it was.
    pub engines: Vec<serde_json::Value>,
    /// The servers left out because no URL was given.
    pub skipped: Vec<&'static str>,
}

#[cfg(feature = "duckdb")]
fn duckdb() -> Box<dyn Engine> {
    Box::new(crate::duckdb::DuckdbEngine)
}

#[cfg(not(feature = "duckdb"))]
fn duckdb() -> Box<dyn Engine> {
    Box::new(crate::absent::Absent::new(
        EngineKind::Duckdb,
        crate::engine::Dialect::Sql,
        "duckdb",
    ))
}

#[cfg(feature = "cozo")]
fn cozo() -> Box<dyn Engine> {
    Box::new(crate::cozo::CozoEngine)
}

#[cfg(not(feature = "cozo"))]
fn cozo() -> Box<dyn Engine> {
    Box::new(crate::absent::Absent::new(
        EngineKind::Cozodb,
        crate::engine::Dialect::Datalog,
        "cozo",
    ))
}

fn engines_for(servers: &Servers) -> Vec<Box<dyn Engine>> {
    vec![
        Box::new(RhizoEngine),
        Box::new(LadybugEngine),
        Box::new(SqliteEngine),
        duckdb(),
        cozo(),
        Box::new(BoltEngine::new(Flavor::Neo4j, servers.neo4j.clone())),
        Box::new(BoltEngine::new(Flavor::Memgraph, servers.memgraph.clone())),
        Box::new(FalkorEngine::new(servers.falkordb.clone())),
    ]
}

/// Runs the suite on every selected engine and returns one report per workload.
pub fn benchmark(options: &Options, progress: impl FnMut(&str)) -> Result<Outcome, String> {
    let plan = options.scale.plan();
    let data = Dataset::generate(SEED, plan);
    let mut environment = Environment::new(&env::temp_dir(), &plan, &data)
        .map_err(|error| format!("cannot create the scratch directory: {error}"))?;
    let mut skipped = Vec::new();
    let all = engines_for(&options.servers);
    let chosen: Vec<&dyn Engine> = all
        .iter()
        .filter(|engine| {
            let kind = engine.kind();
            if options
                .engines
                .as_ref()
                .is_some_and(|only| !only.contains(&kind))
            {
                return false;
            }
            let has_url = match kind {
                EngineKind::Neo4j => options.servers.neo4j.is_some(),
                EngineKind::Memgraph => options.servers.memgraph.is_some(),
                EngineKind::Falkordb => options.servers.falkordb.is_some(),
                _ => true,
            };
            if !has_url {
                skipped.push(kind.name());
            }
            has_url
        })
        .map(|engine| &**engine)
        .collect();
    let settings = Settings {
        reps: options.reps,
        only: options.only.clone(),
        skip: options.skip.clone(),
    };
    let reports = run_all(
        &mut environment,
        &chosen,
        &workloads::all(),
        &settings,
        progress,
    );
    let engines = chosen
        .iter()
        .map(|engine| {
            let kind = engine.kind();
            let mut config = engine.config();
            config["engine"] = json!(kind.name());
            config["category"] = json!(kind.category().name());
            config["storage_kind"] = json!(kind.storage().name());
            config
        })
        .collect();
    Ok(Outcome {
        reports,
        engines,
        skipped,
    })
}

/// The warnings a run's settings call for.
pub fn warnings(reps: usize) -> Vec<String> {
    let mut found = Vec::new();
    if reps < MIN_REPS {
        found.push(format!(
            "WARNING: --reps {reps} is below {MIN_REPS}, so the median and the range are too thin to compare engines"
        ));
    }
    found
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
    let warnings = warnings(arguments.reps);
    for warning in &warnings {
        eprintln!("{warning}");
    }
    let options = Options {
        scale: arguments.scale,
        reps: arguments.reps,
        only: arguments.only.clone(),
        skip: arguments.skip.clone(),
        engines: arguments.engines.clone(),
        servers: arguments.servers.clone(),
    };
    let outcome = match benchmark(&options, |name| eprintln!("running {name}")) {
        Ok(outcome) => outcome,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    let plan = arguments.scale.plan();
    for warning in &warnings {
        println!("{warning}");
    }
    for name in &outcome.skipped {
        println!("skipped {name}: no server URL given");
    }
    print!("{}", table(&outcome.reports));
    if let Some(path) = &arguments.out {
        let header = json!({
            "scale": arguments.scale.name(),
            "reps": arguments.reps,
            "seed": SEED,
            "persons": plan.persons,
            "knows": plan.persons * plan.knows_per_person,
            "cities": plan.cities,
            "cores": std::thread::available_parallelism().map_or(0, |cores| cores.get()),
            "warnings": warnings,
            "skipped_servers": outcome.skipped,
            "engines": outcome.engines,
            "unavailable": unavailable(&outcome.reports)
                .into_iter()
                .map(|(engine, reason, workloads)| json!({"engine": engine, "reason": reason, "workloads": workloads}))
                .collect::<Vec<_>>(),
        });
        let document = json(&outcome.reports, header);
        let text = serde_json::to_string_pretty(&document).unwrap_or_default();
        if let Err(error) = fs::write(path, text) {
            eprintln!("cannot write {}: {error}", path.display());
            return ExitCode::FAILURE;
        }
    }
    let broken = outcome.reports.iter().any(|report| {
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
