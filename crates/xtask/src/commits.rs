use std::error::Error;
use std::fs;
use std::path::Path;
use std::process::Command;

const TYPES: [&str; 11] = [
    "feat", "fix", "chore", "docs", "refactor", "test", "perf", "build", "ci", "style", "revert",
];
const UNCHECKED_PREFIXES: [&str; 3] = ["Merge ", "fixup! ", "squash! "];
const CHANGELOG: &str = "CHANGELOG.md";

#[derive(Debug, PartialEq, Eq)]
pub struct Subject<'a> {
    pub kind: &'a str,
    pub breaking: bool,
}

pub fn parse_subject(subject: &str) -> Option<Subject<'_>> {
    let (head, description) = subject.split_once(": ")?;
    if description.is_empty() || description.starts_with(' ') {
        return None;
    }
    let (head, breaking) = match head.strip_suffix('!') {
        Some(head) => (head, true),
        None => (head, false),
    };
    let kind = match head.split_once('(') {
        None => head,
        Some((kind, scope)) => {
            let scope = scope.strip_suffix(')')?;
            let is_scope_char = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-';
            if scope.is_empty() || !scope.chars().all(is_scope_char) {
                return None;
            }
            kind
        }
    };
    TYPES.contains(&kind).then_some(Subject { kind, breaking })
}

pub fn commit_problems(subject: &str, changed_files: &[&str]) -> Vec<String> {
    if UNCHECKED_PREFIXES
        .iter()
        .any(|prefix| subject.starts_with(prefix))
    {
        return Vec::new();
    }
    let Some(parsed) = parse_subject(subject) else {
        return vec![format!(
            "subject must be `<type>(<scope>)?: <description>` with a type out of {}, got `{subject}`",
            TYPES.join(" ")
        )];
    };
    if (parsed.kind == "feat" || parsed.breaking) && !changed_files.contains(&CHANGELOG) {
        return vec![format!(
            "a feat or breaking commit records its change under Unreleased in {CHANGELOG}, `{subject}` does not touch it"
        )];
    }
    Vec::new()
}

fn git(workspace_root: &Path, args: &[&str]) -> Result<String, Box<dyn Error>> {
    let output = Command::new("git")
        .current_dir(workspace_root)
        .args(args)
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    Ok(String::from_utf8(output.stdout)?)
}

pub fn check_message(
    workspace_root: &Path,
    message_file: Option<&str>,
) -> Result<Vec<String>, Box<dyn Error>> {
    let message_file = message_file.ok_or("usage: xtask commit-msg <message file>")?;
    let message = fs::read_to_string(message_file)?;
    let subject = message.lines().next().unwrap_or_default();
    let staged = git(workspace_root, &["diff", "--cached", "--name-only"])?;
    Ok(commit_problems(
        subject,
        &staged.lines().collect::<Vec<_>>(),
    ))
}

pub fn check_range(
    workspace_root: &Path,
    base: Option<&str>,
) -> Result<Vec<String>, Box<dyn Error>> {
    let base = base.ok_or("usage: xtask commits <base revision>")?;
    let range = format!("{base}..HEAD");
    let commits = git(
        workspace_root,
        &["rev-list", "--no-merges", "--reverse", &range],
    )?;
    let mut violations = Vec::new();
    for commit in commits.lines() {
        let subject = git(workspace_root, &["log", "-1", "--format=%s", commit])?;
        let changed = git(
            workspace_root,
            &["show", "--name-only", "--format=", commit],
        )?;
        violations.extend(
            commit_problems(subject.trim_end(), &changed.lines().collect::<Vec<_>>())
                .into_iter()
                .map(|problem| format!("{}: {problem}", &commit[..commit.len().min(7)])),
        );
    }
    Ok(violations)
}

#[cfg(test)]
#[path = "commits.test.rs"]
mod tests;
