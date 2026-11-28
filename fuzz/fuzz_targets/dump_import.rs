#![no_main]

use libfuzzer_sys::fuzz_target;

const DUMP: &[u8] = include_bytes!("../../crates/rhizodb/tests/fixtures/dump-v1.jsonl");

fn damaged(edits: &[u8]) -> Vec<u8> {
    let mut dump = DUMP.to_vec();
    for edit in edits.chunks(4) {
        let [kind, high, low, value] = edit else { break };
        let at = usize::from(u16::from_be_bytes([*high, *low])) % (dump.len() + 1);
        match kind % 4 {
            0 => {
                if let Some(byte) = dump.get_mut(at) {
                    *byte ^= value;
                }
            }
            1 => dump.truncate(at),
            2 => dump.insert(at, *value),
            _ => {
                if let Some(byte) = dump.get_mut(at) {
                    *byte = *value;
                }
            }
        }
    }
    dump
}

fuzz_target!(|input: &[u8]| {
    let dump = match input.split_first() {
        Some((mode, edits)) if mode % 2 == 1 => damaged(edits),
        _ => input.to_vec(),
    };
    if let Ok(db) = rhizodb::Database::in_memory() {
        let _ = db.import(dump.as_slice());
    }
});
