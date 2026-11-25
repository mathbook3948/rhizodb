#![no_main]

#[path = "../../crates/rhizodb/tests/support/db_file.rs"]
mod db_file;

use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::sync::Mutex;
use std::sync::OnceLock;

use libfuzzer_sys::fuzz_target;

static LAST_PANIC: Mutex<String> = Mutex::new(String::new());

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

fn record_panics_without_aborting() {
    std::panic::set_hook(Box::new(|info| {
        *LAST_PANIC
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = info.to_string();
    }));
}

fuzz_target!(init: record_panics_without_aborting(), |bytes: &[u8]| {
    let file = db_file::damage(pristine().to_vec(), bytes);
    let probed = std::panic::catch_unwind(AssertUnwindSafe(|| {
        db_file::open_and_probe(&directory().path().join("damaged.rhizodb"), &file)
    }));
    if probed.is_err() {
        let last = LAST_PANIC.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        eprintln!("a panic escaped RhizoDB: {last}");
        std::process::abort();
    }
});
