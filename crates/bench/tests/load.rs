use std::env;
use std::time::Duration;

use bench::cli::SEED;
use bench::data::Dataset;
use bench::engine::Engine;
use bench::ladybug::LadybugEngine;
use bench::rhizo::RhizoEngine;
use bench::runner::Environment;
use bench::scale::Scale;
use bench::stress::Load;
use bench::stress::Scenario;
use bench::stress::run_load;

#[test]
fn every_load_scenario_runs_on_rhizodb_and_ladybugdb_with_right_answers() {
    let plan = Scale::Tiny.plan();
    let data = Dataset::generate(SEED, plan);
    let mut environment = Environment::new(&env::temp_dir(), &plan, &data).unwrap();
    let engines: [&dyn Engine; 2] = [&RhizoEngine::default(), &LadybugEngine::default()];
    let load = Load {
        scenarios: Scenario::ALL.to_vec(),
        threads: vec![2],
        duration: Duration::from_millis(700),
    };

    let reports = run_load(&mut environment, &engines, &data, &load, 1, |_| {});

    assert_eq!(reports.len(), 3);
    for report in &reports {
        for series in &report.engines {
            let label = format!("{} on {}", report.point.label(), series.engine.name());
            assert!(
                series.clean(),
                "{label}: {:?} {:?}",
                series.failure,
                series
                    .runs
                    .first()
                    .map(|run| (&run.failure, run.wrong, run.errors))
            );
            let run = series.runs.first().unwrap();
            if report.point.scenario != Scenario::Sustained {
                assert!(run.reads.count > 0, "{label} read nothing");
            }
            if report.point.scenario != Scenario::Readers {
                assert!(run.writes.count > 0, "{label} wrote nothing");
            }
        }
    }
}
