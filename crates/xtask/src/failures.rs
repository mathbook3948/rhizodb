use std::collections::BTreeMap;
use std::error::Error;
use std::fs;
use std::path::Path;

const OUTPUT_LINES: usize = 60;
const OUTPUT_BYTES: usize = 6_000;
const TAIL_LINES: usize = 40;
const SEED_KEY: &str = "RHIZODB_TEST_SEED=";
const SEPARATE_REPORTS: usize = 5;
pub const MAX_BODY: usize = 60_000;

#[derive(Debug, PartialEq)]
pub struct Failure {
    pub binary: String,
    pub test: Option<String>,
    pub seeds: Vec<String>,
    pub output: Vec<String>,
}

#[derive(Debug, PartialEq)]
pub struct Report {
    pub title: String,
    pub body: String,
}

fn header_binary(line: &str) -> Option<String> {
    if let Some(running) = line.strip_prefix("     Running ") {
        let path = running
            .rsplit_once(" (")
            .map_or(running, |(_, path)| path.trim_end_matches(')'));
        let file = path.rsplit(['/', '\\']).next().unwrap_or(path);
        let file = file.strip_suffix(".exe").unwrap_or(file);
        let name = file.rsplit_once('-').map_or(file, |(name, _)| name);
        return Some(name.to_owned());
    }
    line.strip_prefix("   Doc-tests ")
        .map(|krate| format!("{} doctests", krate.trim()))
}

fn seeds_in<'a>(lines: impl Iterator<Item = &'a str>, prefix: &str) -> Vec<String> {
    let key = format!("{prefix}{SEED_KEY}");
    let mut seeds: Vec<String> = lines
        .filter_map(|line| line.split_once(&key).map(|(_, seed)| seed))
        .map(|seed| seed.chars().take_while(char::is_ascii_digit).collect())
        .filter(|seed: &String| !seed.is_empty())
        .collect();
    seeds.sort();
    seeds.dedup();
    seeds
}

pub fn parse(log: &str) -> Vec<Failure> {
    let lines: Vec<&str> = log
        .lines()
        .map(|line| line.trim_end_matches('\r'))
        .collect();
    let mut binary = String::new();
    let mut binary_start = 0;
    let mut failures: Vec<Failure> = Vec::new();
    let mut open: Option<usize> = None;
    for (number, line) in lines.iter().enumerate() {
        if let Some(index) = open {
            if *line == "failures:" || line.starts_with("test result: ") {
                open = None;
            } else if let Some(test) = failed_block(line) {
                open = Some(failure_index(&mut failures, &binary, test));
            } else {
                failures[index].output.push((*line).to_owned());
            }
        } else if let Some(name) = header_binary(line) {
            binary = name;
            binary_start = number;
        } else if let Some(test) = failed_block(line) {
            open = Some(failure_index(&mut failures, &binary, test));
        } else if let Some(test) = line
            .strip_prefix("test ")
            .and_then(|rest| rest.strip_suffix(" ... FAILED"))
        {
            failure_index(&mut failures, &binary, test);
        } else if line.starts_with("error: test failed, to rerun pass ")
            && !failures.iter().any(|failure| failure.binary == binary)
        {
            let start = binary_start.max(number.saturating_sub(TAIL_LINES));
            let output: Vec<String> = lines[start..number]
                .iter()
                .map(|line| (*line).to_owned())
                .collect();
            failures.push(Failure {
                binary: binary.clone(),
                test: None,
                seeds: seeds_in(output.iter().map(String::as_str), ""),
                output,
            });
        }
    }
    for failure in &mut failures {
        if let Some(test) = &failure.test {
            let announced = seeds_in(lines.iter().copied(), &format!("{test} runs with "));
            let printed = seeds_in(failure.output.iter().map(String::as_str), "");
            let mut seeds: Vec<String> = announced.into_iter().chain(printed).collect();
            seeds.sort();
            seeds.dedup();
            failure.seeds = seeds;
        }
    }
    failures
}

fn failed_block(line: &str) -> Option<&str> {
    line.strip_prefix("---- ")
        .and_then(|rest| rest.strip_suffix(" stdout ----"))
}

fn failure_index(failures: &mut Vec<Failure>, binary: &str, test: &str) -> usize {
    match failures
        .iter()
        .position(|failure| failure.binary == binary && failure.test.as_deref() == Some(test))
    {
        Some(index) => index,
        None => {
            failures.push(Failure {
                binary: binary.to_owned(),
                test: Some(test.to_owned()),
                seeds: Vec::new(),
                output: Vec::new(),
            });
            failures.len() - 1
        }
    }
}

fn subject(binary: &str, test: Option<&str>) -> String {
    match test {
        Some(test) => format!("{binary}: {test} fails on main"),
        None => format!("{binary} dies without a test result on main"),
    }
}

fn fit(header: String, sections: &[String]) -> String {
    let mut body = header;
    let mut left_out = 0;
    for section in sections {
        if body.len() + section.len() + 200 <= MAX_BODY {
            body.push_str(section);
        } else {
            left_out += 1;
        }
    }
    if left_out > 0 {
        body.push_str(&format!(
            "\n{left_out} more sections were left out to fit one issue, and the run's logs hold them.\n"
        ));
    }
    body
}

pub fn reports(workflow: &str, logs: &[(String, String)]) -> Vec<Report> {
    if logs.is_empty() {
        return vec![Report {
            title: format!("{workflow} fails on main"),
            body: "No job of the run uploaded a log, so the run page holds the failure.\n"
                .to_owned(),
        }];
    }
    let mut by_test: BTreeMap<(String, Option<String>), Vec<String>> = BTreeMap::new();
    let mut reports = Vec::new();
    for (job, log) in logs {
        let failures = parse(log);
        if failures.is_empty() {
            let lines: Vec<&str> = log.lines().collect();
            let tail = cut(&lines[lines.len().saturating_sub(TAIL_LINES)..]);
            reports.push(Report {
                title: format!("{workflow} job {job} fails on main"),
                body: format!(
                    "The log of `{job}` names no failing test. Its last lines:\n\n```\n{tail}\n```\n"
                ),
            });
        }
        for failure in failures {
            by_test
                .entry((failure.binary.clone(), failure.test.clone()))
                .or_default()
                .push(job_section(job, &failure));
        }
    }
    let separate = by_test.len() <= SEPARATE_REPORTS;
    let mut combined = Vec::new();
    for ((binary, test), sections) in &by_test {
        let header = match test {
            Some(test) => format!("`{test}` in `{binary}` failed in {workflow}.\n"),
            None => format!(
                "`{binary}` exited without reporting a failed test in {workflow}, as a crash or abort does.\n"
            ),
        };
        if separate {
            reports.push(Report {
                title: subject(binary, test.as_deref()),
                body: fit(header, sections),
            });
        } else {
            combined.push(format!(
                "\n## {}\n\n{header}{}",
                test.as_deref().unwrap_or(binary),
                sections.concat()
            ));
        }
    }
    if !separate {
        reports.push(Report {
            title: format!("{workflow}: several tests fail on main"),
            body: fit(
                format!("{} tests failed in {workflow}.\n", by_test.len()),
                &combined,
            ),
        });
    }
    reports
}

fn cut(lines: &[impl AsRef<str>]) -> String {
    let mut shown = String::new();
    let mut kept = 0;
    for line in lines.iter().take(OUTPUT_LINES) {
        let line = line.as_ref();
        if shown.len() + line.len() + 1 > OUTPUT_BYTES {
            let room = OUTPUT_BYTES.saturating_sub(shown.len());
            shown.extend(line.chars().take_while({
                let mut used = 0;
                move |c| {
                    used += c.len_utf8();
                    used <= room
                }
            }));
            shown.push('\n');
            kept += 1;
            break;
        }
        shown.push_str(line);
        shown.push('\n');
        kept += 1;
    }
    let dropped = lines.len() - kept;
    if dropped > 0 {
        shown.push_str(&format!(
            "[{dropped} more lines of output were cut, and the run's logs hold them]\n"
        ));
    }
    shown
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
    let output = cut(&failure.output);
    format!("\n### {job}\n\nSeed: {seeds}\n\n```\n{output}```\n")
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
