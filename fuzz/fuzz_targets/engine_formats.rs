#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    if let Err(problem) = rhizodb::decode_engine_formats_for_fuzzing(bytes) {
        panic!("{problem}");
    }
});
