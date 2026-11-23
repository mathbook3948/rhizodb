use super::*;
use crate::engine::mem_fs::MemFs;
use crate::engine::sim_fs::SimFs;
use crate::storage::conformance;
use crate::storage::conformance::Harness;
use crate::storage::model;
use crate::test_scale;
use proptest::prelude::*;

const NAME: &str = "graph.rhizo";

struct OnSimFs {
    fs: SimFs,
}

impl Harness for OnSimFs {
    type Store = Engine<SimFs>;

    fn open(&mut self) -> Engine<SimFs> {
        Engine::open(Arc::new(self.fs.clone()), NAME, EngineOptions::default()).unwrap()
    }
}

struct InMemory {
    fs: Arc<MemFs>,
}

impl Harness for InMemory {
    type Store = Engine<MemFs>;

    fn open(&mut self) -> Engine<MemFs> {
        Engine::open(Arc::clone(&self.fs), NAME, EngineOptions::default()).unwrap()
    }
}

fn on_sim_fs() -> OnSimFs {
    OnSimFs { fs: SimFs::new() }
}

#[test]
fn the_engine_passes_the_storage_conformance_suite() {
    conformance::run(on_sim_fs);
}

#[test]
fn the_engine_in_memory_passes_the_storage_conformance_suite() {
    conformance::run(|| InMemory {
        fs: Arc::new(MemFs::default()),
    });
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(test_scale::cases(128)))]

    #[test]
    fn the_engine_matches_the_versioned_model(ops in model::ops()) {
        model::check(&mut on_sim_fs(), &ops)?;
    }
}
