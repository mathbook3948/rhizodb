mod docs;
mod forbidden_chars;
mod hooks;

use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let guard = std::env::args().nth(1).unwrap_or_default();

    let violations = match guard.as_str() {
        "forbidden-chars" => forbidden_chars::run(&workspace_root),
        "docs" => docs::check(&workspace_root),
        "docs-write" => docs::write(&workspace_root),
        "hooks" => hooks::check(&workspace_root),
        _ => {
            eprintln!("usage: xtask <forbidden-chars|docs|docs-write|hooks>");
            return ExitCode::from(2);
        }
    };

    match violations {
        Ok(violations) if violations.is_empty() => ExitCode::SUCCESS,
        Ok(violations) => {
            for violation in violations {
                eprintln!("{guard}: {violation}");
            }
            ExitCode::FAILURE
        }
        Err(error) => {
            eprintln!("{guard}: {error}");
            ExitCode::FAILURE
        }
    }
}
