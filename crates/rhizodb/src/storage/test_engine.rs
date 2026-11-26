//! The engine as the graph layer's tests open it: in memory, at a path, or on a simulated file
//! system with inline checkpoints, so a simulated crash is deterministic for its seed.

use std::path::Path;
use std::sync::Arc;

use crate::engine::any_fs;
use crate::engine::any_fs::AnyFs;
use crate::engine::sim_fs::SimFs;
use crate::engine::store::CheckpointMode;
use crate::engine::store::Engine;
use crate::engine::store::EngineOptions;
use crate::storage::StorageError;

pub(crate) type TestStorage = Engine<AnyFs>;

const SIM_NAME: &str = "graph.rhizodb";

pub(crate) fn in_memory() -> TestStorage {
    any_fs::in_memory(EngineOptions::default()).unwrap()
}

pub(crate) fn open(path: impl AsRef<Path>) -> TestStorage {
    any_fs::open_path(path.as_ref(), EngineOptions::default()).unwrap()
}

pub(crate) fn on_sim(fs: &SimFs) -> Result<TestStorage, StorageError> {
    let options = EngineOptions {
        checkpoint: CheckpointMode::Inline,
        ..EngineOptions::default()
    };
    Engine::open(Arc::new(AnyFs::Sim(fs.clone())), SIM_NAME, options).map_err(StorageError::from)
}
