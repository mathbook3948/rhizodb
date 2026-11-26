#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    if let Err(problem) = rhizodb::apply_changes_for_fuzzing(bytes) {
        panic!("{problem}");
    }
});
