#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    let (decoded, skipped) = rhizodb::read_value_for_fuzzing(bytes);
    let Some((encoded, taken)) = decoded else {
        return;
    };
    assert_eq!(
        skipped,
        Some(taken),
        "skipping a value takes other bytes than decoding it"
    );
    let (again, _) = rhizodb::read_value_for_fuzzing(&encoded);
    assert_eq!(
        again,
        Some((encoded.clone(), encoded.len())),
        "a decoded value does not survive encoding and decoding again"
    );
});
