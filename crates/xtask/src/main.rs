mod commits;
mod docs;
mod forbidden_chars;
mod hooks;

use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut args = std::env::args().skip(1);
    let guard = args.next().unwrap_or_default();
    let argument = args.next();

    let violations = match guard.as_str() {
        "commit-msg" => commits::check_message(&workspace_root, argument.as_deref()),
        "commits" => commits::check_range(&workspace_root, argument.as_deref()),
        "forbidden-chars" => forbidden_chars::run(&workspace_root),
        "docs" => docs::check(&workspace_root),
        "docs-write" => docs::write(&workspace_root),
        "hooks" => hooks::check(&workspace_root),
        _ => {
            eprintln!("usage: xtask <commit-msg|commits|forbidden-chars|docs|docs-write|hooks>");
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
