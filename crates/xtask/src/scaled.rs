use std::error::Error;
use std::fs;
use std::path::Path;
use std::process::Command;

const MISE: &str = "mise.toml";
const LIB_TASK: &str = "storage-deep";
const INTEGRATION_TASK: &str = "random-deep";
const CRATE_ROOT: &str = "crates/rhizodb";
const SOURCE_ROOT: &str = "crates/rhizodb/src/";
const TEST_ROOT: &str = "crates/rhizodb/tests/";
const SCALE_HELPER: &str = "crates/rhizodb/src/test_scale.rs";
const SCALE_ENV: &str = "RHIZODB_TEST_SCALE";

pub struct Source<'a> {
    pub path: &'a str,
    pub text: &'a str,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Selection<'a> {
    pub filters: Vec<&'a str>,
    pub skips: Vec<&'a str>,
    pub tests: Vec<&'a str>,
}

pub fn task_commands<'a>(mise: &'a str, task: &str) -> Option<Vec<&'a str>> {
    let header = format!("[tasks.{task}]");
    let mut lines = mise.lines().skip_while(|line| *line != header);
    lines.next()?;
    Some(
        lines
            .take_while(|line| !line.starts_with('['))
            .filter_map(|line| {
                let (_, rest) = line.split_once("\"cargo test ")?;
                rest.split_once('"').map(|(command, _)| command)
            })
            .collect(),
    )
}

pub fn selection<'a>(commands: &[&'a str]) -> Selection<'a> {
    let mut selection = Selection::default();
    for command in commands {
        let (cargo_args, test_args) = command.split_once(" -- ").unwrap_or((command, ""));
        let mut cargo_args = cargo_args.split_whitespace();
        while let Some(arg) = cargo_args.next() {
            if arg == "--test" {
                selection.tests.extend(cargo_args.next());
            }
        }
        let mut test_args = test_args.split_whitespace();
        while let Some(arg) = test_args.next() {
            if arg == "--skip" {
                selection.skips.extend(test_args.next());
            } else if !arg.starts_with('-') {
                selection.filters.push(arg);
            }
        }
    }
    selection
}

fn natural_module(path: &str) -> String {
    let relative = path.strip_prefix(SOURCE_ROOT).unwrap_or(path);
    let relative = relative.strip_suffix(".rs").unwrap_or(relative);
    let relative = relative.strip_suffix("/mod").unwrap_or(relative);
    if relative == "lib" {
        return String::new();
    }
    relative.replace('/', "::")
}

fn child_module(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_string()
    } else {
        format!("{parent}::{name}")
    }
}

fn path_declarations(source: &Source) -> Vec<(String, String)> {
    let directory = source.path.rsplit_once('/').map_or("", |(dir, _)| dir);
    let module = natural_module(source.path);
    let mut declarations = Vec::new();
    let mut pending = None;
    for line in source.text.lines().map(str::trim) {
        if let Some(target) = line
            .strip_prefix("#[path = \"")
            .and_then(|rest| rest.strip_suffix("\"]"))
        {
            pending = Some(target);
            continue;
        }
        if line.starts_with("#[") {
            continue;
        }
        let Some(target) = pending.take() else {
            continue;
        };
        if let Some(name) = line
            .split_once("mod ")
            .and_then(|(_, rest)| rest.strip_suffix(';'))
        {
            declarations.push((format!("{directory}/{target}"), child_module(&module, name)));
        }
    }
    declarations
}

fn calls_scaled_cases(text: &str) -> bool {
    text.contains(SCALE_ENV)
        || text.match_indices("cases(").any(|(at, _)| {
            !text[..at]
                .chars()
                .next_back()
                .is_some_and(|before| before.is_alphanumeric() || before == '_')
        })
}

pub fn unscaled_problems(
    sources: &[Source],
    lib: &Selection,
    integration: &Selection,
) -> Vec<String> {
    let declarations: Vec<(String, String)> = sources.iter().flat_map(path_declarations).collect();
    let mut problems = Vec::new();
    for source in sources {
        if source.path.starts_with(SOURCE_ROOT) {
            if source.path == SCALE_HELPER || !source.text.contains("test_scale::") {
                continue;
            }
            let declared = declarations
                .iter()
                .find(|(target, _)| target == source.path)
                .map(|(_, module)| module.clone());
            let Some(module) = declared.or_else(|| {
                (!source.path.ends_with(".test.rs")).then(|| natural_module(source.path))
            }) else {
                problems.push(format!(
                    "{} scales its cases with {SCALE_ENV}, but no `#[path]` declares it as a module",
                    source.path
                ));
                continue;
            };
            let prefix = format!("{module}::");
            let selected = lib.filters.iter().any(|filter| prefix.contains(filter))
                && !lib.skips.iter().any(|skip| prefix.contains(skip));
            if !selected {
                problems.push(format!(
                    "{} scales its cases with {SCALE_ENV}, but `mise run {LIB_TASK}` does not select `{module}`, so the nightly never runs it at scale",
                    source.path
                ));
            }
        } else if let Some(name) = source
            .path
            .strip_prefix(TEST_ROOT)
            .and_then(|file| file.strip_suffix(".rs"))
            .filter(|name| !name.contains('/'))
            && calls_scaled_cases(source.text)
            && !integration.tests.contains(&name)
        {
            problems.push(format!(
                "{} scales its cases with {SCALE_ENV}, but `mise run {INTEGRATION_TASK}` does not pass `--test {name}`, so the nightly never runs it at scale",
                source.path
            ));
        }
    }
    problems
}

pub fn check(workspace_root: &Path) -> Result<Vec<String>, Box<dyn Error>> {
    let mise = fs::read_to_string(workspace_root.join(MISE))?;
    let commands = |task: &str| {
        task_commands(&mise, task).ok_or_else(|| format!("{MISE} has no [tasks.{task}]"))
    };
    let lib = selection(&commands(LIB_TASK)?);
    let integration = selection(&commands(INTEGRATION_TASK)?);
    let listing = Command::new("git")
        .current_dir(workspace_root)
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
            CRATE_ROOT,
        ])
        .output()?;
    if !listing.status.success() {
        return Err("git ls-files failed".into());
    }
    let paths = String::from_utf8(listing.stdout)?;
    let mut texts = Vec::new();
    for path in paths.split('\0').filter(|path| path.ends_with(".rs")) {
        if let Ok(text) = fs::read_to_string(workspace_root.join(path)) {
            texts.push((path, text));
        }
    }
    let sources: Vec<Source> = texts
        .iter()
        .map(|(path, text)| Source { path, text })
        .collect();
    Ok(unscaled_problems(&sources, &lib, &integration))
}

#[cfg(test)]
#[path = "scaled.test.rs"]
mod tests;
