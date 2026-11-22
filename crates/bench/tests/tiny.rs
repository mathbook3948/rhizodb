use bench::cli::benchmark;
use bench::report::Agreement;
use bench::scale::Scale;

const OPTIONAL_ON_LADYBUG: [&str; 4] = [
    "index/keyword-create",
    "index/vector-create",
    "index/keyword-search",
    "index/vector-search",
];

#[test]
fn every_workload_runs_on_both_engines_without_errors_and_with_agreeing_results() {
    let reports = benchmark(Scale::Tiny, 1, None, |_| {}).unwrap();
    let mut problems = Vec::new();
    for report in &reports {
        let optional = OPTIONAL_ON_LADYBUG.contains(&report.name);
        for series in &report.series {
            let ladybug_may_skip = optional && series.unsupported.is_some();
            if ladybug_may_skip {
                continue;
            }
            if series.runs.is_empty() {
                problems.push(format!(
                    "{} on {}: no result ({:?} {:?})",
                    report.name,
                    series.engine.name(),
                    series.failure,
                    series.unsupported
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
        }
        for (series, agreement) in report.series.iter().zip(&report.agreements) {
            match agreement {
                Agreement::Disagree(detail) => problems.push(format!("{}: {detail}", report.name)),
                Agreement::NotComparable(why) if !optional => problems.push(format!(
                    "{} on {}: not comparable, {why}",
                    report.name,
                    series.engine.name()
                )),
                _ => {}
            }
        }
    }

    assert!(problems.is_empty(), "{}", problems.join("\n"));
    assert!(reports.len() >= 20, "{}", reports.len());
}
