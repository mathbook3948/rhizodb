use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use tck::engine::Unimplemented;

fn main() -> ExitCode {
    tck::cli::main(
        Arc::new(Unimplemented),
        Path::new(env!("CARGO_MANIFEST_DIR")),
    )
}
