use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;

use libtest_mimic::Arguments;
use libtest_mimic::Failed;
use libtest_mimic::Trial;

use crate::engine::Engine;
use crate::ratchet::Ratchet;
use crate::suite::Verdict;
use crate::suite::evaluate;
use crate::suite::load_cases;

/// Runs the whole TCK against `engine` as a libtest-style binary.
///
/// `crate_root` holds `vendor/opencypher` and `passing.txt`. With `TCK_BLESS=1` every scenario
/// runs and `passing.txt` is rewritten with the ones that pass.
pub fn main(engine: Arc<dyn Engine>, crate_root: &Path) -> ExitCode {
    let args = Arguments::from_args();
    let bless = std::env::var("TCK_BLESS").is_ok_and(|value| value == "1");
    if bless && (args.filter.is_some() || !args.skip.is_empty()) {
        eprintln!("tck: refusing to bless with a name filter, it would drop unlisted scenarios");
        return ExitCode::FAILURE;
    }
    let vendor = crate_root.join("vendor/opencypher");
    let ratchet_path = crate_root.join("passing.txt");
    let cases = match load_cases(&vendor) {
        Ok(cases) => cases,
        Err(error) => {
            eprintln!("tck: {error}");
            return ExitCode::FAILURE;
        }
    };
    let ratchet = match fs::read_to_string(&ratchet_path) {
        Ok(text) => Ratchet::parse(&text),
        Err(error) => {
            eprintln!("tck: cannot read {}: {error}", ratchet_path.display());
            return ExitCode::FAILURE;
        }
    };
    let existing: BTreeSet<&str> = cases.iter().map(|case| case.name.as_str()).collect();
    let stale = ratchet.stale(&existing);
    if !bless && !stale.is_empty() {
        for name in stale {
            eprintln!("tck: passing.txt lists a scenario that does not exist: {name}");
        }
        return ExitCode::FAILURE;
    }

    let total = cases.len();
    let ratchet = Arc::new(ratchet);
    let passing = Arc::new(Mutex::new(BTreeSet::new()));
    let trials: Vec<Trial> = cases
        .into_iter()
        .map(|case| {
            let ignored = case.tags.contains("ignore");
            let name = case.name.clone();
            let engine = Arc::clone(&engine);
            let ratchet = Arc::clone(&ratchet);
            let passing = Arc::clone(&passing);
            let vendor = vendor.clone();
            Trial::test(name, move || {
                let verdict = evaluate(engine.as_ref(), &case, &vendor);
                if verdict == Verdict::Passed {
                    passing
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .insert(case.name.clone());
                }
                if bless {
                    return match verdict {
                        Verdict::HarnessError(message) => Err(Failed::from(message)),
                        Verdict::Passed | Verdict::Failed(_) => Ok(()),
                    };
                }
                ratchet.judge(&case.name, &verdict).map_err(Failed::from)
            })
            .with_ignored_flag(ignored)
        })
        .collect();

    let conclusion = libtest_mimic::run(&args, trials);
    let passing = passing
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    println!("tck: {}/{total} scenarios passing", passing.len());
    if !bless {
        return conclusion.exit_code();
    }
    if conclusion.has_failed() {
        eprintln!("tck: not writing passing.txt because of harness errors");
        return ExitCode::FAILURE;
    }
    match fs::write(&ratchet_path, Ratchet::from_passing(passing).render()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("tck: cannot write {}: {error}", ratchet_path.display());
            ExitCode::FAILURE
        }
    }
}
