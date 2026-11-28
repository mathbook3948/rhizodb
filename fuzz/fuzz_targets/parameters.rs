#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    if let Err(problem) = rhizodb::run_with_parameters_for_fuzzing(bytes) {
        panic!("{problem}");
    }
});
