use bench::cli::Options;
use bench::cli::Servers;
use bench::cli::benchmark;
use bench::engine::EngineKind;
use bench::report::Agreement;
use bench::scale::Scale;

/// What an engine has no form of, which the report shows as unsupported instead of a failure.
const UNSUPPORTED: [(EngineKind, &str); 5] = [
    (EngineKind::Sqlite, "index/vector-create"),
    (EngineKind::Sqlite, "index/vector-search"),
    (EngineKind::Duckdb, "index/keyword-create"),
    (EngineKind::Duckdb, "index/vector-create"),
    (EngineKind::Cozodb, "path/all-shortest-pair"),
];

fn embedded() -> Vec<EngineKind> {
    let mut engines = vec![
        EngineKind::Rhizodb,
        EngineKind::Ladybugdb,
        EngineKind::Sqlite,
    ];
    if cfg!(feature = "duckdb") {
        engines.push(EngineKind::Duckdb);
    }
    if cfg!(feature = "cozo") {
        engines.push(EngineKind::Cozodb);
    }
    engines
}

#[test]
fn every_workload_runs_on_every_embedded_engine_without_errors_and_with_agreeing_results() {
    let options = Options {
        scale: Scale::Tiny,
        reps: 1,
        only: None,
        skip: None,
        engines: Some(embedded()),
        servers: Servers::default(),
    };
    let reports = benchmark(&options, |_| {}).unwrap().reports;
    let mut problems = Vec::new();
    for report in &reports {
        for series in &report.series {
            let allowed = UNSUPPORTED.contains(&(series.engine, report.name));
            if let Some(reason) = &series.unavailable {
                problems.push(format!(
                    "{} on {}: unavailable, {reason}",
                    report.name,
                    series.engine.name()
                ));
            } else if series.unsupported.is_some() && !allowed {
                problems.push(format!(
                    "{} on {}: unsupported, {:?}",
                    report.name,
                    series.engine.name(),
                    series.unsupported
                ));
            } else if series.runs.is_empty() && !allowed {
                problems.push(format!(
                    "{} on {}: no result, {:?}",
                    report.name,
                    series.engine.name(),
                    series.failure
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
            if series.engine == EngineKind::Rhizodb && report.name.contains("-search") {
                let recall = series.runs.first().and_then(|run| run.recall);
                if recall != Some(1.0) {
                    problems.push(format!("{}: rhizodb recall is {recall:?}", report.name));
                }
            }
        }
        for (series, agreement) in report.series.iter().zip(&report.agreements) {
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
    assert!(reports.len() >= 20, "{}", reports.len());
}
