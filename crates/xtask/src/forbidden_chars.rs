use std::error::Error;
use std::path::Path;
use std::process::Command;

const VENDORED_PREFIXES: [&str; 3] = [
    ".claude/skills/",
    "crates/tck/vendor/",
    "crates/tck/passing.txt",
];

const FORBIDDEN: [(char, &str); 13] = [
    ('\u{2013}', "en dash"),
    ('\u{2014}', "em dash"),
    ('\u{00B7}', "middle dot"),
    ('\u{318D}', "hangul middle dot"),
    ('\u{30FB}', "katakana middle dot"),
    ('\u{3008}', "left angle bracket"),
    ('\u{3009}', "right angle bracket"),
    ('\u{300A}', "left double angle bracket"),
    ('\u{300B}', "right double angle bracket"),
    ('\u{300C}', "left corner bracket"),
    ('\u{300D}', "right corner bracket"),
    ('\u{300E}', "left white corner bracket"),
    ('\u{300F}', "right white corner bracket"),
];

#[derive(Debug, PartialEq, Eq)]
pub struct ForbiddenChar {
    pub line: usize,
    pub name: &'static str,
}

pub fn forbidden_chars_in(text: &str) -> Vec<ForbiddenChar> {
    text.lines()
        .enumerate()
        .flat_map(|(index, line)| {
            line.chars().filter_map(move |candidate| {
                FORBIDDEN
                    .iter()
                    .find(|(forbidden, _)| *forbidden == candidate)
                    .map(|(_, name)| ForbiddenChar {
                        line: index + 1,
                        name,
                    })
            })
        })
        .collect()
}

pub fn is_vendored(path: &str) -> bool {
    VENDORED_PREFIXES
        .iter()
        .any(|prefix| path.starts_with(prefix))
}

pub fn run(workspace_root: &Path) -> Result<Vec<String>, Box<dyn Error>> {
    let listing = Command::new("git")
        .current_dir(workspace_root)
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .output()?;
    if !listing.status.success() {
        return Err("git ls-files failed".into());
    }

    let mut violations = Vec::new();
    let project_files = String::from_utf8(listing.stdout)?;
    for path in project_files
        .split('\0')
        .filter(|path| !path.is_empty() && !is_vendored(path))
    {
        let Ok(bytes) = std::fs::read(workspace_root.join(path)) else {
            continue;
        };
        let Ok(text) = String::from_utf8(bytes) else {
            continue;
        };
        violations.extend(
            forbidden_chars_in(&text)
                .into_iter()
                .map(|found| format!("{path}:{}: {}", found.line, found.name)),
        );
    }
    Ok(violations)
}

#[cfg(test)]
#[path = "forbidden_chars.test.rs"]
mod tests;
