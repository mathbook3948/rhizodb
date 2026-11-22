use std::error::Error;
use std::fs;
use std::path::Path;
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
    if String::from_utf8(hooks_path.stdout)?.trim() != HOOKS_DIR {
        violations.push(format!(
            "git hooks are not installed, run `mise install` to set core.hooksPath to {HOOKS_DIR}"
        ));
    }
    Ok(violations)
}

#[cfg(test)]
#[path = "hooks.test.rs"]
mod tests;
