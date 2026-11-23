use std::error::Error;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

const HOOKS_DIR: &str = ".githooks";
const SG_CONFIG: &str = "sgconfig.yml";

pub fn bash_globs(config: &str) -> Vec<&str> {
    config
        .lines()
        .skip_while(|line| *line != "languageGlobs:")
        .skip_while(|line| line.trim_end() != "  bash:")
        .skip(1)
        .map_while(|line| line.strip_prefix("    - "))
        .map(|glob| glob.trim().trim_matches('"'))
        .collect()
}

pub fn unscanned_hooks<'a>(hooks: &'a [String], config: &str) -> Vec<&'a str> {
    let globs = bash_globs(config);
    hooks
        .iter()
        .map(String::as_str)
        .filter(|hook| !globs.contains(hook))
        .collect()
}

pub fn check(workspace_root: &Path) -> Result<Vec<String>, Box<dyn Error>> {
    let mut hooks = Vec::new();
    for entry in fs::read_dir(workspace_root.join(HOOKS_DIR))? {
        hooks.push(entry?.file_name().to_string_lossy().into_owned());
    }
    let config = fs::read_to_string(workspace_root.join(SG_CONFIG))?;

    let mut violations: Vec<String> = unscanned_hooks(&hooks, &config)
        .into_iter()
        .map(|hook| {
            format!("{HOOKS_DIR}/{hook} is missing from languageGlobs.bash in {SG_CONFIG}, so ast-grep never scans it")
        })
        .collect();

    let hooks_path = Command::new("git")
        .current_dir(workspace_root)
        .args(["config", "--get", "core.hooksPath"])
        .output()?;
    let common_dir = Command::new("git")
        .current_dir(workspace_root)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .output()?;
    let common_dir = PathBuf::from(String::from_utf8(common_dir.stdout)?.trim());
    let installed = String::from_utf8(hooks_path.stdout)?;
    if !installs_hooks(installed.trim(), workspace_root, common_dir.parent()) {
        violations.push(format!(
            "git hooks are not installed, run `mise install` to set core.hooksPath to {HOOKS_DIR}"
        ));
    }
    Ok(violations)
}

/// Whether a `core.hooksPath` value points at this repository's hooks directory.
///
/// Tools that create linked worktrees may rewrite the relative value into an absolute path to the
/// main worktree's hooks directory, which runs the same hooks.
pub fn installs_hooks(value: &str, workspace_root: &Path, main_root: Option<&Path>) -> bool {
    if value == HOOKS_DIR {
        return true;
    }
    let value = Path::new(value);
    if !value.is_absolute() {
        return false;
    }
    let Ok(value) = value.canonicalize() else {
        return false;
    };
    std::iter::once(workspace_root)
        .chain(main_root)
        .filter_map(|root| root.join(HOOKS_DIR).canonicalize().ok())
        .any(|hooks| hooks == value)
}

#[cfg(test)]
#[path = "hooks.test.rs"]
mod tests;
