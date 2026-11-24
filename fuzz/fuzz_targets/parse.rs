#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: &str| {
    rhizodb::parse_for_fuzzing(input);
});
