use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    tck::kuzu::main(
        &root.join("vendor/kuzu/test_files"),
        &root.join("kuzu/passing.txt"),
    )
}
