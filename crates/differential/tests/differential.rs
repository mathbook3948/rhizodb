use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    differential::cli::main(Path::new(env!("CARGO_MANIFEST_DIR")))
}
