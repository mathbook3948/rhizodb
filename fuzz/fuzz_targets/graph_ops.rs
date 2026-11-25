#![no_main]

#[path = "../../crates/rhizodb/tests/support/graph_ops.rs"]
#[allow(dead_code, reason = "reopening a file is for the integration test, which runs on disk")]
mod graph_ops;

use graph_ops::ByteChoices;
use graph_ops::Choices;
use graph_ops::Run;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    let mut choices = ByteChoices::new(bytes);
    let mut run = Run::new(rhizodb::Database::in_memory().unwrap());
    while !choices.exhausted() {
        if let Err(reason) = run.transaction(&mut choices) {
            panic!("{reason}\nstatements:\n{}", run.log());
        }
    }
});
