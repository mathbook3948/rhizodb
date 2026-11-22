//! Prints one tab-separated line per TCK scenario: trial name, verdict, and failure message.
//!
//! Run with `cargo run -p tck --example report`, then group the messages to find what to fix.

use std::path::Path;
use std::process::ExitCode;

use tck::rhizo::RhizoDb;
use tck::suite::Verdict;
use tck::suite::evaluate;
use tck::suite::load_cases;

fn main() -> ExitCode {
    let vendor = Path::new(env!("CARGO_MANIFEST_DIR")).join("vendor/opencypher");
    let cases = match load_cases(&vendor) {
        Ok(cases) => cases,
        Err(error) => {
            eprintln!("report: {error}");
            return ExitCode::FAILURE;
        }
    };
    for case in cases {
        let (verdict, message) = match evaluate(&RhizoDb, &case, &vendor) {
            Verdict::Passed => ("passed", String::new()),
            Verdict::Failed(message) => ("failed", message),
            Verdict::HarnessError(message) => ("harness", message),
        };
        let message = message.replace(['\n', '\t'], " ");
        println!("{}\t{verdict}\t{message}", case.name);
    }
    ExitCode::SUCCESS
}
