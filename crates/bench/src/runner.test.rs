use std::path::Path;
use std::sync::Mutex;

use super::*;
use crate::data::Dataset;
use crate::engine::Session;
use crate::fingerprint::Fingerprint;
use crate::report::Agreement;
use crate::scale::Scale;
use crate::workloads::Comparison;

static LOG: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());

struct Fake(EngineKind);

struct FakeInstance;

impl Engine for Fake {
    fn kind(&self) -> EngineKind {
        self.0
    }

    fn dialect(&self) -> crate::engine::Dialect {
        crate::engine::Dialect::Cypher
    }

    fn open(&self, _dir: &Path) -> Result<Box<dyn Instance>, EngineError> {
        Ok(Box::new(FakeInstance))
    }
}

impl Instance for FakeInstance {
    fn session(&self) -> Result<Box<dyn Session + '_>, EngineError> {
        Err(EngineError::Failed("no sessions".to_owned()))
    }

    fn create_schema(&self) -> Result<(), EngineError> {
        Ok(())
    }

    fn prepare_bulk_load(&self, _: &Dataset, _: &Path) -> Result<(), EngineError> {
        Ok(())
    }

    fn bulk_load(&self, _: &Dataset, _: &Path) -> Result<(), EngineError> {
        Ok(())
    }

    fn create_keyword_index(&self) -> Result<(), EngineError> {
        Ok(())
    }

    fn create_vector_index(&self) -> Result<(), EngineError> {
        Ok(())
    }

    fn keyword_search(&self) -> &'static str {
        ""
    }

    fn vector_search(&self) -> &'static str {
        ""
    }
}

fn logging(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    LOG.lock().unwrap().push(ctx.engine.kind().name());
    Ok(Measurement {
        ops: 1,
        fingerprint: Fingerprint {
            rows: 1,
            checksum: 5,
        },
        ..Measurement::default()
    })
}

fn declining(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    match ctx.engine.kind() {
        EngineKind::Ladybugdb => Err(EngineError::Unsupported("no extension".to_owned())),
        EngineKind::Rhizodb => logging(ctx),
    }
}

fn workload(name: &'static str, run: crate::workloads::Run) -> Workload {
    Workload {
        name,
        needs: Needs::Empty,
        comparison: Comparison::Exact,
        run,
    }
}

#[test]
fn engines_alternate_after_an_unreported_warm_up_and_unsupported_work_is_recorded() {
    let plan = Scale::Tiny.plan();
    let data = Dataset::generate(1, plan);
    let mut environment = Environment::new(&std::env::temp_dir(), &plan, &data).unwrap();
    let rhizo = Fake(EngineKind::Rhizodb);
    let ladybug = Fake(EngineKind::Ladybugdb);
    let engines: [&dyn Engine; 2] = [&rhizo, &ladybug];
    let settings = Settings {
        reps: 3,
        only: Some("alternate".to_owned()),
        seed: 1,
    };
    let workloads = [
        workload("x/alternate", logging),
        workload("x/skipped", logging),
    ];

    let reports = run_all(&mut environment, &engines, &workloads, &settings, |_| {});

    assert_eq!(
        *LOG.lock().unwrap(),
        [
            "rhizodb",
            "ladybugdb",
            "rhizodb",
            "ladybugdb",
            "ladybugdb",
            "rhizodb",
            "rhizodb",
            "ladybugdb"
        ]
    );
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].series[0].runs.len(), 3);
    assert_eq!(reports[0].series[1].runs.len(), 3);
    assert_eq!(reports[0].agreements, [Agreement::Agree, Agreement::Agree]);

    let settings = Settings {
        reps: 2,
        only: Some("declined".to_owned()),
        seed: 1,
    };
    let reports = run_all(
        &mut environment,
        &engines,
        &[workload("x/declined", declining)],
        &settings,
        |_| {},
    );

    assert_eq!(reports[0].series[0].runs.len(), 2);
    assert!(reports[0].series[1].runs.is_empty());
    assert_eq!(
        reports[0].series[1].unsupported.as_deref(),
        Some("no extension")
    );
    assert!(matches!(
        reports[0].agreements[1],
        Agreement::NotComparable(_)
    ));
}

#[test]
fn the_scratch_directory_is_removed_when_the_environment_drops() {
    let plan = Scale::Tiny.plan();
    let data = Dataset::generate(1, plan);
    let root = {
        let environment = Environment::new(&std::env::temp_dir(), &plan, &data).unwrap();
        let root = environment.root_for_test();
        assert!(root.exists());
        root
    };

    assert!(!root.exists());
}
