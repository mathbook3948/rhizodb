use bench::cli::Options;
use bench::cli::Servers;
use bench::cli::benchmark;
use bench::engine::EngineKind;
use bench::memory::Memory;
use bench::report::Agreement;
use bench::scale::Scale;

/// What an engine has no form of, which the report shows as unsupported instead of a failure.
const UNSUPPORTED: [(EngineKind, &str); 4] = [
    (EngineKind::Sqlite, "index/vector-create"),
    (EngineKind::Sqlite, "index/vector-search"),
    (EngineKind::Duckdb, "index/keyword-create"),
    (EngineKind::Duckdb, "index/vector-create"),
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
        memory: Memory::default(),
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

#[cfg(target_os = "linux")]
#[test]
fn reads_from_a_small_cache_and_a_cold_page_cache_agree_and_read_the_disk() {
    let options = Options {
        scale: Scale::Tiny,
        reps: 1,
        only: Some("read/,traverse/,aggregate/,index/vector-search".to_owned()),
        skip: None,
        engines: Some(vec![EngineKind::Rhizodb, EngineKind::Ladybugdb]),
        servers: Servers::default(),
        memory: Memory {
            cache_bytes: Some(32 << 20),
            cold: true,
        },
    };
    let reports = benchmark(&options, |_| {}).unwrap().reports;

    assert!(reports.len() >= 10, "{}", reports.len());
    for report in &reports {
        assert!(report.disagreements().is_empty(), "{}", report.name);
        for series in &report.series {
            assert_eq!(series.runs.len(), 1, "{} on {:?}", report.name, series);
            assert_eq!(series.runs[0].errors, 0, "{} on {:?}", report.name, series);
        }
    }
    let scan = reports
        .iter()
        .find(|report| report.name == "read/filter-scan")
        .unwrap();
    for series in &scan.series {
        let read = series.runs[0].disk_read_bytes.unwrap_or_default();
        assert!(read > 0, "{} read nothing from disk", series.engine.name());
    }
}
