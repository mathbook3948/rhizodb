#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    if let Err(problem) = rhizodb::install_damaged_snapshot_for_fuzzing(bytes) {
        panic!("{problem}");
    }
});
