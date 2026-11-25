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
    pub skip: Option<String>,
}

struct Indexed {
    instance: Box<dyn Instance>,
    state: IndexState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Held {
    Loaded,
    Indexed,
    Dirty,
}

/// The scratch directory of a run and the databases built in it.
///
/// For an engine that keeps its data in a directory, it builds the loaded graph once, as a
/// master that nothing opens afterwards. Read workloads share one opened copy of the master, and
/// every workload that changes the graph or times an open starts from its own fresh copy. An
/// engine whose state lives in a server has no directory to copy, so a workload that changes the
/// state is followed by clearing the server and loading the graph again before the next one that
/// needs it.
pub struct Environment<'a> {
    root: PathBuf,
    plan: &'a Plan,
    data: &'a Dataset,
    masters: BTreeMap<EngineKind, Result<PathBuf, EngineError>>,
    bases: BTreeMap<EngineKind, Box<dyn Instance>>,
    indexed: BTreeMap<EngineKind, Indexed>,
    held: BTreeMap<EngineKind, Held>,
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
        self.bases.clear();
        self.indexed.clear();
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
            masters: BTreeMap::new(),
            bases: BTreeMap::new(),
            indexed: BTreeMap::new(),
            held: BTreeMap::new(),
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

    fn master(&mut self, engine: &dyn Engine) -> Result<PathBuf, EngineError> {
        let kind = engine.kind();
        if let Some(cached) = self.masters.get(&kind) {
            return cached.clone();
        }
        let built = self.build_master(engine);
        self.masters.insert(kind, built.clone());
        built
    }

    fn build_master(&self, engine: &dyn Engine) -> Result<PathBuf, EngineError> {
        let dir = self.engine_dir(engine.kind(), "master");
        let building = self.engine_dir(engine.kind(), "master.building");
        let _ = fs::remove_dir_all(&building);
        fs::create_dir_all(&building).map_err(io_failure)?;
        let scratch = self.scratch();
        fs::create_dir_all(&scratch).map_err(io_failure)?;
        let instance = engine.open(&building)?;
        instance.create_schema()?;
        instance.prepare_bulk_load(self.data, &scratch)?;
        instance.bulk_load(self.data, &scratch)?;
        instance.close()?;
        fs::rename(&building, &dir).map_err(io_failure)?;
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
        if self.bases.contains_key(&engine.kind()) {
            return Ok(());
        }
        let master = self.master(engine)?;
        let dir = self.engine_dir(engine.kind(), "base");
        copy_directory(&master, &dir).map_err(io_failure)?;
        self.bases.insert(engine.kind(), engine.open(&dir)?);
        Ok(())
    }

    fn ensure_indexed(&mut self, engine: &dyn Engine) -> Result<(), EngineError> {
        if self.indexed.contains_key(&engine.kind()) {
            return Ok(());
        }
        let master = self.master(engine)?;
        let dir = self.engine_dir(engine.kind(), "indexed");
        copy_directory(&master, &dir).map_err(io_failure)?;
        let instance = engine.open(&dir)?;
        let state = build_indexes(&*instance);
        self.indexed
            .insert(engine.kind(), Indexed { instance, state });
        Ok(())
    }

    fn ensure_held(&mut self, engine: &dyn Engine, want: Held) -> Result<(), EngineError> {
        let kind = engine.kind();
        let held = self.held.get(&kind).copied();
        let enough = matches!(
            (held, want),
            (Some(Held::Indexed), Held::Loaded | Held::Indexed)
                | (Some(Held::Loaded), Held::Loaded)
        );
        if enough {
            return Ok(());
        }
        self.bases.remove(&kind);
        self.indexed.remove(&kind);
        self.held.insert(kind, Held::Dirty);
        let scratch = self.scratch();
        fs::create_dir_all(&scratch).map_err(io_failure)?;
        let dir = self.engine_dir(kind, "server");
        fs::create_dir_all(&dir).map_err(io_failure)?;
        let instance = engine.open(&dir)?;
        instance.reset()?;
        instance.create_schema()?;
        instance.prepare_bulk_load(self.data, &scratch)?;
        instance.bulk_load(self.data, &scratch)?;
        if want == Held::Indexed {
            let state = build_indexes(&*instance);
            self.indexed.insert(kind, Indexed { instance, state });
        } else {
            self.bases.insert(kind, instance);
        }
        self.held.insert(kind, want);
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
        let kind = engine.kind();
        let shared = engine.shared_state();
        if shared && workload.file_only {
            return Err(EngineError::Unsupported(
                "the server keeps running, so there is no database file to open".to_owned(),
            ));
        }
        match workload.needs {
            Needs::Base => {
                if shared {
                    self.ensure_held(engine, Held::Loaded)?;
                } else {
                    self.ensure_base(engine)?;
                }
                let instance = self
                    .bases
                    .get(&kind)
                    .or_else(|| self.indexed.get(&kind).map(|indexed| &indexed.instance));
                let dir = self.engine_dir(kind, if shared { "server" } else { "base" });
                (workload.run)(&self.context(engine, instance.map(|b| &**b), None, &dir, &scratch))
            }
            Needs::Indexed => {
                if shared {
                    self.ensure_held(engine, Held::Indexed)?;
                } else {
                    self.ensure_indexed(engine)?;
                }
                let indexed = self.indexed.get(&kind);
                let dir = self.engine_dir(kind, if shared { "server" } else { "indexed" });
                let instance = indexed.map(|i| &*i.instance);
                let state = indexed.map(|i| &i.state);
                (workload.run)(&self.context(engine, instance, state, &dir, &scratch))
            }
            Needs::Copy | Needs::ClosedCopy | Needs::Empty => {
                let dir = if shared {
                    self.held.insert(kind, Held::Dirty);
                    self.bases.remove(&kind);
                    self.indexed.remove(&kind);
                    let dir = self.engine_dir(kind, "server");
                    fs::create_dir_all(&dir).map_err(io_failure)?;
                    if workload.needs == Needs::Empty {
                        engine.open(&dir)?.reset()?;
                    } else {
                        self.ensure_held(engine, Held::Loaded)?;
                        self.held.insert(kind, Held::Dirty);
                    }
                    dir
                } else if workload.needs == Needs::Empty {
                    self.runs += 1;
                    let dir = self.engine_dir(kind, &format!("run-{}", self.runs));
                    fs::create_dir_all(&dir).map_err(io_failure)?;
                    dir
                } else {
                    self.copy_of_master(engine)?
                };
                let outcome = self.run_in(engine, workload, &dir, &scratch);
                if !shared {
                    let _ = fs::remove_dir_all(&dir);
                }
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

fn build_indexes(instance: &dyn Instance) -> IndexState {
    match instance.prepare_search() {
        Ok(()) => IndexState {
            keyword: instance.create_keyword_index(),
            vector: instance.create_vector_index(),
        },
        Err(error) => IndexState {
            keyword: Err(error.clone()),
            vector: Err(error),
        },
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
            fs::copy(entry.path(), &target)?;
            fs::File::open(&target)?.sync_all()?;
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
            && settings
                .skip
                .as_deref()
                .is_none_or(|skip| !workload.name.contains(skip))
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
                unavailable: None,
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
            EngineError::Unavailable(reason) => series.unavailable = Some(reason),
            EngineError::Failed(message) => series.failure = Some(message),
        }
    }
}

#[cfg(test)]
#[path = "runner.test.rs"]
mod tests;
