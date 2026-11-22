use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use tck::rhizo::RhizoDb;

fn main() -> ExitCode {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("gql");
    tck::cli::main(Arc::new(RhizoDb), &root, &root.join("passing.txt"))
}
