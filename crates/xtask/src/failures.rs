use std::collections::BTreeMap;
use std::error::Error;
use std::fs;
use std::path::Path;

const OUTPUT_LINES: usize = 60;
const TAIL_LINES: usize = 40;
const SEED_KEY: &str = "RHIZODB_TEST_SEED=";
const SEPARATE_REPORTS: usize = 5;

#[derive(Debug, PartialEq)]
pub struct Failure {
    pub target: String,
    pub test: String,
    pub seeds: Vec<String>,
    pub output: Vec<String>,
}

#[derive(Debug, PartialEq)]
pub struct Report {
    pub title: String,
    pub body: String,
}

pub fn parse(log: &str) -> Vec<Failure> {
    let lines: Vec<&str> = log
        .lines()
        .map(|line| line.trim_end_matches('\r'))
        .collect();
    let mut target = String::new();
    let mut failures: Vec<Failure> = Vec::new();
    let mut open: Option<usize> = None;
    for line in &lines {
        if let Some(running) = line.trim_start().strip_prefix("Running ") {
            target = running
                .split(" (")
                .next()
                .unwrap_or(running)
                .trim()
                .to_owned();
            open = None;
        } else if let Some(test) = line
            .strip_prefix("---- ")
            .and_then(|rest| rest.strip_suffix(" stdout ----"))
        {
            open = Some(failure_index(&mut failures, &target, test));
        } else if *line == "failures:" || line.starts_with("test result: ") {
            open = None;
        } else if let Some(test) = line
            .strip_prefix("test ")
            .and_then(|rest| rest.strip_suffix(" ... FAILED"))
        {
            failure_index(&mut failures, &target, test);
        } else if let Some(index) = open {
            failures[index].output.push((*line).to_owned());
        }
    }
    for failure in &mut failures {
        let announced = format!("{} runs with {SEED_KEY}", failure.test);
        let mut seeds: Vec<String> = lines
            .iter()
            .filter_map(|line| line.split_once(&announced).map(|(_, seed)| seed))
            .chain(
                failure
                    .output
                    .iter()
                    .filter_map(|line| line.split_once(SEED_KEY).map(|(_, seed)| seed)),
            )
            .map(|seed| seed.chars().take_while(char::is_ascii_digit).collect())
            .filter(|seed: &String| !seed.is_empty())
            .collect();
        seeds.sort();
        seeds.dedup();
        failure.seeds = seeds;
    }
    failures
}

fn failure_index(failures: &mut Vec<Failure>, target: &str, test: &str) -> usize {
    match failures
        .iter()
        .position(|failure| failure.target == target && failure.test == test)
    {
        Some(index) => index,
        None => {
            failures.push(Failure {
                target: target.to_owned(),
                test: test.to_owned(),
                seeds: Vec::new(),
                output: Vec::new(),
            });
            failures.len() - 1
        }
    }
}

pub fn reports(workflow: &str, logs: &[(String, String)]) -> Vec<Report> {
    if logs.is_empty() {
        return vec![Report {
            title: format!("{workflow} fails on main"),
            body: "No job of the run uploaded a log, so the run page holds the failure.\n"
                .to_owned(),
        }];
    }
    let mut by_test: BTreeMap<(String, String), String> = BTreeMap::new();
    let mut reports = Vec::new();
    for (job, log) in logs {
        let failures = parse(log);
        if failures.is_empty() {
            let lines: Vec<&str> = log.lines().collect();
            let tail = lines[lines.len().saturating_sub(TAIL_LINES)..].join("\n");
            reports.push(Report {
                title: format!("{workflow} job {job} fails on main"),
                body: format!(
                    "The log of `{job}` names no failing test. Its last lines:\n\n```\n{tail}\n```\n"
                ),
            });
        }
        for failure in failures {
            let section = by_test
                .entry((failure.target.clone(), failure.test.clone()))
                .or_default();
            section.push_str(&job_section(job, &failure));
        }
    }
    let separate = by_test.len() <= SEPARATE_REPORTS;
    let mut combined = String::new();
    for ((target, test), sections) in &by_test {
        let body = format!("`{test}` in `{target}` failed in {workflow}.\n{sections}");
        if separate {
            reports.push(Report {
                title: format!("{test} fails on main"),
                body,
            });
        } else {
            combined.push_str(&format!("\n## {test}\n\n{body}"));
        }
    }
    if !separate {
        reports.push(Report {
            title: format!("{workflow}: {} tests fail on main", by_test.len()),
            body: combined,
        });
    }
    reports
}

fn job_section(job: &str, failure: &Failure) -> String {
    let seeds = match failure.seeds.as_slice() {
        [] => "The log names no seed.".to_owned(),
        seeds => seeds
            .iter()
            .map(|seed| format!("`{SEED_KEY}{seed}`"))
            .collect::<Vec<_>>()
            .join(", "),
    };
    let shown = failure.output.len().min(OUTPUT_LINES);
    let output = failure.output[..shown].join("\n");
    format!("\n### {job}\n\nSeed: {seeds}\n\n```\n{output}\n```\n")
}

pub fn run(
    logs: Option<&str>,
    out: Option<&str>,
    workflow: Option<&str>,
) -> Result<Vec<String>, Box<dyn Error>> {
    let (Some(logs), Some(out), Some(workflow)) = (logs, out, workflow) else {
        return Err("usage: xtask failures <logs directory> <reports directory> <workflow>".into());
    };
    let mut collected = Vec::new();
    if Path::new(logs).is_dir() {
        let mut jobs: Vec<_> = fs::read_dir(logs)?.collect::<Result<_, _>>()?;
        jobs.sort_by_key(|entry| entry.file_name());
        for job in jobs {
            let name = job.file_name().to_string_lossy().into_owned();
            let name = name.strip_prefix("log-").unwrap_or(&name).to_owned();
            let mut files: Vec<_> = fs::read_dir(job.path())?.collect::<Result<_, _>>()?;
            files.sort_by_key(|entry| entry.file_name());
            for file in files {
                let bytes = fs::read(file.path())?;
                collected.push((name.clone(), String::from_utf8_lossy(&bytes).into_owned()));
            }
        }
    }
    fs::create_dir_all(out)?;
    for (index, report) in reports(workflow, &collected).iter().enumerate() {
        fs::write(
            Path::new(out).join(format!("{index}.md")),
            format!("{}\n{}", report.title, report.body),
        )?;
    }
    Ok(Vec::new())
}

#[cfg(test)]
#[path = "failures.test.rs"]
mod tests;
