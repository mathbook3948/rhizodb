use bench::cli::Options;
use bench::cli::Servers;
use bench::cli::benchmark;
use bench::engine::EngineKind;
use bench::report::Agreement;
use bench::scale::Scale;

fn url(variable: &str) -> Option<String> {
    std::env::var(variable)
        .ok()
        .filter(|value| !value.is_empty())
}

/// Runs every workload at tiny scale on each server whose URL the environment gives, with
/// `BENCH_NEO4J_URL`, `BENCH_MEMGRAPH_URL`, and `BENCH_FALKORDB_URL`. It does nothing without any.
#[test]
fn every_workload_runs_on_the_servers_the_environment_names_with_agreeing_results() {
    let servers = Servers {
        neo4j: url("BENCH_NEO4J_URL"),
        memgraph: url("BENCH_MEMGRAPH_URL"),
        falkordb: url("BENCH_FALKORDB_URL"),
    };
    let mut engines = vec![EngineKind::Rhizodb];
    for (kind, given) in [
        (EngineKind::Neo4j, &servers.neo4j),
        (EngineKind::Memgraph, &servers.memgraph),
        (EngineKind::Falkordb, &servers.falkordb),
    ] {
        if given.is_some() {
            engines.push(kind);
        }
    }
    if engines.len() == 1 {
        eprintln!("skipped: set BENCH_NEO4J_URL, BENCH_MEMGRAPH_URL, or BENCH_FALKORDB_URL");
        return;
    }
    let options = Options {
        scale: Scale::Tiny,
        reps: 1,
        only: None,
        skip: None,
        engines: Some(engines),
        servers,
    };
    let reports = benchmark(&options, |_| {}).unwrap().reports;
    let mut problems = Vec::new();
    for report in &reports {
        for (series, agreement) in report.series.iter().zip(&report.agreements) {
            if series.engine == EngineKind::Rhizodb {
                continue;
            }
            if series.runs.is_empty() && series.unsupported.is_none() {
                problems.push(format!(
                    "{} on {}: no result, {:?} {:?}",
                    report.name,
                    series.engine.name(),
                    series.failure,
                    series.unavailable
                ));
            }
            for run in &series.runs {
                if run.errors > 0 {
                    problems.push(format!(
                        "{} on {}: {} errors, {:?}",
                        report.name,
                        series.engine.name(),
                        run.errors,
                        run.failure
                    ));
                }
            }
            if let Agreement::Disagree(detail) = agreement {
                problems.push(format!(
                    "{} on {}: {detail}",
                    report.name,
                    series.engine.name()
                ));
            }
        }
    }

    assert!(problems.is_empty(), "{}", problems.join("\n"));
}
