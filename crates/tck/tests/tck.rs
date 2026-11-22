use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use tck::rhizo::RhizoDb;

fn main() -> ExitCode {
    tck::cli::main(Arc::new(RhizoDb), Path::new(env!("CARGO_MANIFEST_DIR")))
}
