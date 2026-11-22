use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::process::ExitCode;

use libtest_mimic::Arguments;
use libtest_mimic::Failed;
use libtest_mimic::Trial;
use tck::suite::load_cases;

use crate::corpus::Query;
use crate::corpus::from_tck;
use crate::corpus::generated;
use crate::corpus::hand_written;
use crate::expectations::Expectations;
use crate::graph::Graph;
use crate::graph::Size;
use crate::run::run_all;

/// Seed of the generated graph and queries. Changing it changes the corpus.
pub const SEED: u64 = 20_261_122;

/// Size of the generated graph.
pub const GRAPH_SIZE: Size = Size {
    people: 40,
    knows: 80,
};

/// Number of generated queries.
pub const GENERATED_QUERIES: usize = 400;

/// Runs the differential suite as a libtest-style binary.
///
/// `crate_root` holds `corpus.txt` and `differences.txt`, and the TCK crate sits next to it.
/// Every query runs before any trial is judged, so a name filter only narrows what is judged.
/// With `DIFFERENTIAL_BLESS=1` the whole corpus is judged and `differences.txt` is rewritten
/// with every query on which the engines do not agree.
pub fn main(crate_root: &Path) -> ExitCode {
    match run(crate_root) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("differential: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(crate_root: &Path) -> Result<ExitCode, String> {
    let args = Arguments::from_args();
    let bless = std::env::var("DIFFERENTIAL_BLESS").is_ok_and(|value| value == "1");
    if bless && (args.filter.is_some() || !args.skip.is_empty()) {
        return Err("refusing to bless a filtered run, it would drop queries".into());
    }
    let read = |path: &Path| {
        fs::read_to_string(path).map_err(|error| format!("cannot read {}: {error}", path.display()))
    };
    let tck = crate_root.join("../tck");
    let cases = load_cases(&tck.join("vendor/opencypher")).map_err(|error| error.to_string())?;
    let passing: BTreeSet<String> = read(&tck.join("passing.txt"))?
        .lines()
        .map(str::to_string)
        .collect();
    let mut queries: Vec<Query> = hand_written(&read(&crate_root.join("corpus.txt"))?);
    queries.extend(generated(SEED, GENERATED_QUERIES));
    queries.extend(from_tck(&cases, &passing));
    let mut names = BTreeSet::new();
    if let Some(query) = queries
        .iter()
        .find(|query| !names.insert(query.name.as_str()))
    {
        return Err(format!("query listed twice: {}", query.name));
    }

    let expectations_path = crate_root.join("differences.txt");
    let expectations =
        Expectations::parse(&read(&expectations_path)?).map_err(|error| error.to_string())?;
    let stale = expectations.stale(&names);
    if !bless && !stale.is_empty() {
        return Err(format!(
            "differences.txt lists queries that are not in the corpus: {stale:?}"
        ));
    }

    let outcomes = run_all(&queries, &Graph::generate(SEED, GRAPH_SIZE))?;
    let agreeing = outcomes
        .iter()
        .filter(|outcome| outcome.kind() == "agree")
        .count();
    println!(
        "differential: the engines agree on {agreeing} of {} queries",
        queries.len()
    );
    if bless {
        let blessed = expectations.bless(
            queries
                .iter()
                .map(|query| query.name.as_str())
                .zip(&outcomes),
        );
        fs::write(&expectations_path, blessed.render())
            .map_err(|error| format!("cannot write {}: {error}", expectations_path.display()))?;
        return Ok(ExitCode::SUCCESS);
    }
    let trials = queries
        .into_iter()
        .zip(outcomes)
        .map(|(query, outcome)| {
            let verdict = expectations.judge(&query.name, &outcome);
            Trial::test(query.name, move || verdict.map_err(Failed::from))
        })
        .collect();
    Ok(libtest_mimic::run(&args, trials).exit_code())
}
