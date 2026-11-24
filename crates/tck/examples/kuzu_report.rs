//! Prints one tab-separated line per Kuzu case: case name, verdict, and failure message.
//!
//! Run with `cargo run -p tck --example kuzu_report`, then group the messages to find what to fix.

use std::path::Path;
use std::process::ExitCode;

use tck::kuzu::evaluate;
use tck::kuzu::load;
use tck::suite::Verdict;

fn main() -> ExitCode {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("vendor/kuzu/test_files");
    let cases = match load(&root) {
        Ok(cases) => cases,
        Err(error) => {
            eprintln!("kuzu_report: {error}");
            return ExitCode::FAILURE;
        }
    };
    for case in cases {
        let (verdict, message) = match evaluate(&case) {
            Verdict::Passed => ("passed", String::new()),
            Verdict::Failed(message) => ("failed", message),
            Verdict::HarnessError(message) => ("harness", message),
        };
        let message = message.replace(['\n', '\t'], " ");
        println!("{}\t{verdict}\t{message}", case.name);
    }
    ExitCode::SUCCESS
}
