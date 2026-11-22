use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use crate::data::Dataset;
use crate::engine::Engine;
use crate::engine::EngineError;
use crate::engine::EngineKind;
use crate::engine::Instance;
use crate::measure::Measurement;
use crate::report::Series;
use crate::report::WorkloadReport;
use crate::report::agreements;
use crate::scale::Plan;
use crate::workloads::Context;
use crate::workloads::IndexState;
use crate::workloads::Needs;
use crate::workloads::Workload;
use crate::workloads::io_failure;

/// What a benchmark run does.
#[derive(Clone, Debug)]
pub struct Settings {
    pub reps: usize,
    pub only: Option<String>,
    pub seed: u64,
}

struct Indexed {
    instance: Box<dyn Instance>,
    state: IndexState,
}

/// The scratch directory of a run and the databases built in it.
///
/// It builds each engine's loaded graph once, as a master that nothing opens afterwards. Read
/// workloads share one opened copy of the master, and every workload that changes the graph or
/// times an open starts from its own fresh copy.
pub struct Environment<'a> {
    root: PathBuf,
    plan: &'a Plan,
    data: &'a Dataset,
    bases: BTreeMap<&'static str, Box<dyn Instance>>,
    indexed: BTreeMap<&'static str, Indexed>,
    runs: usize,
}

impl fmt::Debug for Environment<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Environment")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

impl Drop for Environment<'_> {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

impl<'a> Environment<'a> {
    /// Creates a scratch directory under `parent` for one run.
    pub fn new(parent: &Path, plan: &'a Plan, data: &'a Dataset) -> io::Result<Self> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        let root = parent.join(format!("rhizodb-bench-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&root)?;
        Ok(Environment {
            root,
            plan,
            data,
            bases: BTreeMap::new(),
            indexed: BTreeMap::new(),
            runs: 0,
        })
    }

    #[cfg(test)]
    pub(crate) fn root_for_test(&self) -> PathBuf {
        self.root.clone()
    }

    fn engine_dir(&self, kind: EngineKind, name: &str) -> PathBuf {
        self.root.join(kind.name()).join(name)
    }

    fn scratch(&self) -> PathBuf {
        self.root.join("input")
    }

    fn master(&self, engine: &dyn Engine) -> Result<PathBuf, EngineError> {
        let dir = self.engine_dir(engine.kind(), "master");
        if dir.exists() {
            return Ok(dir);
        }
        fs::create_dir_all(&dir).map_err(io_failure)?;
        let scratch = self.scratch();
        fs::create_dir_all(&scratch).map_err(io_failure)?;
        let instance = engine.open(&dir)?;
        instance.create_schema()?;
        instance.prepare_bulk_load(self.data, &scratch)?;
        instance.bulk_load(self.data, &scratch)?;
        drop(instance);
        Ok(dir)
    }

    fn copy_of_master(&mut self, engine: &dyn Engine) -> Result<PathBuf, EngineError> {
        let master = self.master(engine)?;
        self.runs += 1;
        let dir = self.engine_dir(engine.kind(), &format!("run-{}", self.runs));
        copy_directory(&master, &dir).map_err(io_failure)?;
        Ok(dir)
    }

    fn ensure_base(&mut self, engine: &dyn Engine) -> Result<(), EngineError> {
        if self.bases.contains_key(engine.kind().name()) {
            return Ok(());
        }
        let master = self.master(engine)?;
        let dir = self.engine_dir(engine.kind(), "base");
        copy_directory(&master, &dir).map_err(io_failure)?;
        self.bases.insert(engine.kind().name(), engine.open(&dir)?);
        Ok(())
    }

    fn ensure_indexed(&mut self, engine: &dyn Engine) -> Result<(), EngineError> {
        if self.indexed.contains_key(engine.kind().name()) {
            return Ok(());
        }
        let master = self.master(engine)?;
        let dir = self.engine_dir(engine.kind(), "indexed");
        copy_directory(&master, &dir).map_err(io_failure)?;
        let instance = engine.open(&dir)?;
        let state = IndexState {
            keyword: instance.create_keyword_index(),
            vector: instance.create_vector_index(),
        };
        self.indexed
            .insert(engine.kind().name(), Indexed { instance, state });
        Ok(())
    }

    fn context<'c>(
        &'c self,
        engine: &'c dyn Engine,
        instance: Option<&'c dyn Instance>,
        indexes: Option<&'c IndexState>,
        dir: &'c Path,
        scratch: &'c Path,
    ) -> Context<'c> {
        Context {
            engine,
            instance,
            indexes,
            data: self.data,
            plan: self.plan,
            dir,
            scratch,
        }
    }

    /// Runs `workload` once on `engine` from the state it needs.
    pub fn run_once(
        &mut self,
        engine: &dyn Engine,
        workload: &Workload,
    ) -> Result<Measurement, EngineError> {
        let scratch = self.scratch();
        fs::create_dir_all(&scratch).map_err(io_failure)?;
        match workload.needs {
            Needs::Base => {
                self.ensure_base(engine)?;
                let instance = self.bases.get(engine.kind().name()).map(|b| &**b);
                let dir = self.engine_dir(engine.kind(), "base");
                (workload.run)(&self.context(engine, instance, None, &dir, &scratch))
            }
            Needs::Indexed => {
                self.ensure_indexed(engine)?;
                let indexed = self.indexed.get(engine.kind().name());
                let dir = self.engine_dir(engine.kind(), "indexed");
                let instance = indexed.map(|i| &*i.instance);
                let state = indexed.map(|i| &i.state);
                (workload.run)(&self.context(engine, instance, state, &dir, &scratch))
            }
            Needs::Copy | Needs::ClosedCopy | Needs::Empty => {
                let dir = if workload.needs == Needs::Empty {
                    self.runs += 1;
                    let dir = self.engine_dir(engine.kind(), &format!("run-{}", self.runs));
                    fs::create_dir_all(&dir).map_err(io_failure)?;
                    dir
                } else {
                    self.copy_of_master(engine)?
                };
                let outcome = self.run_in(engine, workload, &dir, &scratch);
                let _ = fs::remove_dir_all(&dir);
                outcome
            }
        }
    }

    fn run_in(
        &self,
        engine: &dyn Engine,
        workload: &Workload,
        dir: &Path,
        scratch: &Path,
    ) -> Result<Measurement, EngineError> {
        if workload.needs == Needs::Copy {
            let instance = engine.open(dir)?;
            (workload.run)(&self.context(engine, Some(&*instance), None, dir, scratch))
        } else {
            (workload.run)(&self.context(engine, None, None, dir, scratch))
        }
    }
}

fn copy_directory(from: &Path, to: &Path) -> io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.metadata()?.is_dir() {
            copy_directory(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

/// Runs every selected workload: one warm-up run per engine that is not reported, then `reps`
/// repetitions that alternate which engine goes first.
pub fn run_all(
    environment: &mut Environment<'_>,
    engines: &[&dyn Engine],
    workloads: &[Workload],
    settings: &Settings,
    mut progress: impl FnMut(&str),
) -> Vec<WorkloadReport> {
    let selected = workloads.iter().filter(|workload| {
        settings
            .only
            .as_deref()
            .is_none_or(|only| workload.name.contains(only))
    });
    let mut reports = Vec::new();
    for workload in selected {
        progress(workload.name);
        let mut series: Vec<Series> = engines
            .iter()
            .map(|engine| Series {
                engine: engine.kind(),
                runs: Vec::new(),
                unsupported: None,
                failure: None,
            })
            .collect();
        let mut alive = vec![true; engines.len()];
        for (index, engine) in engines.iter().enumerate() {
            if let Err(error) = environment.run_once(*engine, workload) {
                alive[index] = false;
                record_error(series.get_mut(index), error);
            }
        }
        for rep in 0..settings.reps {
            let order: Vec<usize> = if rep % 2 == 0 {
                (0..engines.len()).collect()
            } else {
                (0..engines.len()).rev().collect()
            };
            for index in order {
                let (Some(engine), Some(true)) = (engines.get(index), alive.get(index)) else {
                    continue;
                };
                match environment.run_once(*engine, workload) {
                    Ok(measurement) => {
                        if let Some(series) = series.get_mut(index) {
                            series.runs.push(measurement);
                        }
                    }
                    Err(error) => {
                        if let Some(flag) = alive.get_mut(index) {
                            *flag = false;
                        }
                        record_error(series.get_mut(index), error);
                    }
                }
            }
        }
        let agreements = agreements(&series, workload.comparison);
        reports.push(WorkloadReport {
            name: workload.name,
            series,
            agreements,
        });
    }
    reports
}

fn record_error(series: Option<&mut Series>, error: EngineError) {
    if let Some(series) = series {
        match error {
            EngineError::Unsupported(reason) => series.unsupported = Some(reason),
            EngineError::Failed(message) => series.failure = Some(message),
        }
    }
}

#[cfg(test)]
#[path = "runner.test.rs"]
mod tests;
