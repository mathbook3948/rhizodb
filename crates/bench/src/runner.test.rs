use std::path::Path;
use std::sync::Mutex;

use super::*;
use crate::data::Dataset;
use crate::engine::Built;
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

    fn config(&self) -> serde_json::Value {
        serde_json::json!({})
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

    fn create_keyword_index(&self) -> Result<Built, EngineError> {
        Ok(Built::Index)
    }

    fn create_vector_index(&self) -> Result<Built, EngineError> {
        Ok(Built::Index)
    }

    fn keyword_search(&self) -> Result<&'static str, EngineError> {
        Ok("")
    }

    fn vector_search(&self) -> Result<&'static str, EngineError> {
        Ok("")
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
        _ => logging(ctx),
    }
}

fn workload(name: &'static str, run: crate::workloads::Run) -> Workload {
    Workload {
        name,
        needs: Needs::Empty,
        comparison: Comparison::Exact,
        file_only: false,
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
        skip: None,
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
        skip: None,
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

struct Failing {
    opened: Mutex<usize>,
}

impl Engine for Failing {
    fn kind(&self) -> EngineKind {
        EngineKind::Sqlite
    }

    fn dialect(&self) -> crate::engine::Dialect {
        crate::engine::Dialect::Sql
    }

    fn open(&self, dir: &Path) -> Result<Box<dyn Instance>, EngineError> {
        *self.opened.lock().unwrap() += 1;
        std::fs::write(dir.join("half-loaded"), b"x").unwrap();
        Ok(Box::new(BrokenLoad))
    }

    fn config(&self) -> serde_json::Value {
        serde_json::json!({})
    }
}

struct BrokenLoad;

impl Instance for BrokenLoad {
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
        Err(EngineError::Failed("the load died".to_owned()))
    }

    fn create_keyword_index(&self) -> Result<Built, EngineError> {
        Ok(Built::Index)
    }

    fn create_vector_index(&self) -> Result<Built, EngineError> {
        Ok(Built::Index)
    }

    fn keyword_search(&self) -> Result<&'static str, EngineError> {
        Ok("")
    }

    fn vector_search(&self) -> Result<&'static str, EngineError> {
        Ok("")
    }
}

#[test]
fn a_failed_load_is_remembered_and_never_leaves_a_master_directory_to_reuse() {
    let plan = Scale::Tiny.plan();
    let data = Dataset::generate(1, plan);
    let mut environment = Environment::new(&std::env::temp_dir(), &plan, &data).unwrap();
    let engine = Failing {
        opened: Mutex::new(0),
    };
    let workload = Workload {
        name: "x/copy",
        needs: Needs::Copy,
        comparison: Comparison::Exact,
        file_only: false,
        run: logging,
    };

    let first = environment.run_once(&engine, &workload);
    let second = environment.run_once(&engine, &workload);

    assert_eq!(
        first.unwrap_err(),
        EngineError::Failed("the load died".to_owned())
    );
    assert_eq!(
        second.unwrap_err(),
        EngineError::Failed("the load died".to_owned())
    );
    assert_eq!(*engine.opened.lock().unwrap(), 1);
    assert!(
        !environment
            .root_for_test()
            .join("sqlite")
            .join("master")
            .exists()
    );
}
