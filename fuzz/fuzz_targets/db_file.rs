#![no_main]

#[path = "../../crates/rhizodb/tests/support/db_file.rs"]
mod db_file;

use std::path::Path;
use std::sync::OnceLock;

use libfuzzer_sys::fuzz_target;

fn directory() -> &'static tempfile::TempDir {
    static DIRECTORY: OnceLock<tempfile::TempDir> = OnceLock::new();
    DIRECTORY.get_or_init(|| {
        let shm = Path::new("/dev/shm");
        if shm.is_dir() {
            tempfile::tempdir_in(shm).unwrap()
        } else {
            tempfile::tempdir().unwrap()
        }
    })
}

fn pristine() -> &'static [u8] {
    static PRISTINE: OnceLock<Vec<u8>> = OnceLock::new();
    PRISTINE.get_or_init(|| db_file::pristine(&directory().path().join("pristine.rhizodb")))
}

fuzz_target!(|bytes: &[u8]| {
    let file = db_file::damage(pristine().to_vec(), bytes);
    db_file::open_and_probe(&directory().path().join("damaged.rhizodb"), &file);
    if let Err(reason) = rhizodb::read_damaged_relationship_chunks_for_fuzzing(bytes) {
        panic!("{reason}");
    }
});
