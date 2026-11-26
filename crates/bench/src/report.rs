use std::time::Duration;

use serde_json::Value;
use serde_json::json;

use crate::engine::Category;
use crate::engine::EngineKind;
use crate::engine::Storage;
use crate::measure::Measurement;
use crate::workloads::Comparison;

/// Median and range of a set of durations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stats {
    pub median: Duration,
    pub min: Duration,
    pub max: Duration,
}

impl Stats {
    /// The statistics of `samples`, or `None` for no samples.
    pub fn of(samples: &[Duration]) -> Option<Stats> {
        let mut sorted = samples.to_vec();
        sorted.sort();
        let middle = sorted.len() / 2;
        let median = if sorted.len() % 2 == 1 {
            *sorted.get(middle)?
        } else {
            (*sorted.get(middle.checked_sub(1)?)? + *sorted.get(middle)?) / 2
        };
        Some(Stats {
            median,
            min: *sorted.first()?,
            max: *sorted.last()?,
        })
    }
}

/// What one engine did on one workload across all its repetitions.
#[derive(Clone, Debug)]
pub struct Series {
    pub engine: EngineKind,
    pub runs: Vec<Measurement>,
    pub unsupported: Option<String>,
    pub unavailable: Option<String>,
    pub failure: Option<String>,
}

/// Whether the two engines produced the same result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Agreement {
    Agree,
    Disagree(String),
    NotComparable(String),
}

/// Every engine's series for one workload with each engine's verdict against the reference.
#[derive(Clone, Debug)]
pub struct WorkloadReport {
    pub name: &'static str,
    pub series: Vec<Series>,
    pub agreements: Vec<Agreement>,
}

impl WorkloadReport {
    /// The details of every engine whose result differs from the reference.
    pub fn disagreements(&self) -> Vec<&str> {
        self.agreements
            .iter()
            .filter_map(|agreement| match agreement {
                Agreement::Disagree(detail) => Some(detail.as_str()),
                _ => None,
            })
            .collect()
    }
}

/// Compares every engine that produced a result with the reference, which is RhizoDB when it
/// produced one and the first engine with a result otherwise. One verdict per series, in order.
/// The reference is judged against its own repetitions.
pub fn agreements(series: &[Series], comparison: Comparison) -> Vec<Agreement> {
    if comparison == Comparison::GroundTruth {
        return series
            .iter()
            .map(|_| {
                Agreement::NotComparable("judged by recall against the exact answer".to_owned())
            })
            .collect();
    }
    let key = |run: &Measurement| (run.fingerprint.rows, run.fingerprint.checksum);
    let reference = series
        .iter()
        .find(|s| s.engine == EngineKind::Rhizodb && !s.runs.is_empty())
        .or_else(|| series.iter().find(|s| !s.runs.is_empty()));
    let Some(expected) = reference.and_then(|s| s.runs.first()).map(key) else {
        return series
            .iter()
            .map(|_| Agreement::NotComparable("no engine produced a result".to_owned()))
            .collect();
    };
    let reference_engine = reference.map(|s| s.engine);
    series
        .iter()
        .map(|candidate| {
            if candidate.runs.is_empty() {
                return Agreement::NotComparable("no result".to_owned());
            }
            match candidate.runs.iter().map(key).find(|found| *found != expected) {
                None => Agreement::Agree,
                Some(found) => Agreement::Disagree(format!(
                    "{} returned rows={} checksum={:x}, reference {} returned rows={} checksum={:x}",
                    candidate.engine.name(),
                    found.0,
                    found.1,
                    reference_engine.map_or("none", EngineKind::name),
                    expected.0,
                    expected.1
                )),
            }
        })
        .collect()
}

fn duration_text(duration: Duration) -> String {
    let seconds = duration.as_secs_f64();
    if seconds < 1e-3 {
        format!("{:.1}us", seconds * 1e6)
    } else if seconds < 1.0 {
        format!("{:.2}ms", seconds * 1e3)
    } else {
        format!("{seconds:.2}s")
    }
}

fn size_text(bytes: u64) -> String {
    format!("{:.1}MiB", bytes as f64 / (1024.0 * 1024.0))
}

fn per_second(ops: u64, elapsed: Duration) -> Option<f64> {
    let seconds = elapsed.as_secs_f64();
    (ops > 1 && seconds > 0.0).then(|| ops as f64 / seconds)
}

fn samples(series: &Series) -> Vec<Duration> {
    series.runs.iter().map(|run| run.elapsed).collect()
}

fn errors(series: &Series) -> u64 {
    series.runs.iter().map(|run| run.errors).sum()
}

fn first_line(message: &str) -> String {
    let line = message.lines().next().unwrap_or_default();
    match line.char_indices().nth(110) {
        Some((end, _)) => format!("{}...", &line[..end]),
        None => line.to_owned(),
    }
}

fn recall(series: &Series) -> Option<f64> {
    series
        .runs
        .iter()
        .filter_map(|run| run.recall)
        .min_by(f64::total_cmp)
}

fn notes(workload: &str, series: &Series) -> String {
    let mut notes = Vec::new();
    match series.runs.first().and_then(|run| run.bytes) {
        Some(bytes) => notes.push(format!("size {}", size_text(bytes))),
        None if workload == "load/bulk" => notes.push("size n/a".to_owned()),
        None => {}
    }
    if let Some(close) = series
        .runs
        .iter()
        .map(|run| run.close)
        .filter(|close| !close.is_zero())
        .min()
    {
        notes.push(format!("incl. close {}", duration_text(close)));
    }
    let mut disk: Vec<u64> = series
        .runs
        .iter()
        .filter_map(|run| run.disk_read_bytes)
        .collect();
    disk.sort_unstable();
    if let Some(&median) = disk.get(disk.len() / 2).filter(|bytes| **bytes > 0) {
        notes.push(format!("disk read {}", size_text(median)));
    }
    if let Some(note) = series.runs.iter().find_map(|run| run.note.clone()) {
        notes.push(note);
    }
    if let Some(message) = series.runs.iter().find_map(|run| run.failure.clone()) {
        notes.push(first_line(&message));
    }
    notes.join("; ")
}

fn engine_lines(reports: &[WorkloadReport]) -> Vec<String> {
    let mut lines = Vec::new();
    for category in [Category::Embedded, Category::Server] {
        let engines: Vec<String> = EngineKind::ALL
            .into_iter()
            .filter(|kind| kind.category() == category)
            .filter(|kind| {
                reports.iter().any(|report| {
                    report
                        .series
                        .iter()
                        .any(|series| series.engine == *kind && !series.runs.is_empty())
                })
            })
            .map(|kind| format!("{} ({})", kind.name(), kind.storage().name()))
            .collect();
        if !engines.is_empty() {
            lines.push(format!("{}: {}", category.name(), engines.join(", ")));
        }
    }
    lines
}

/// The table printed on stdout.
pub fn table(reports: &[WorkloadReport]) -> String {
    let header = [
        "workload", "engine", "kind", "median", "range", "ops/s", "rows", "errors", "agree",
        "recall", "notes",
    ];
    let mut lines: Vec<Vec<String>> = vec![header.iter().map(|text| (*text).to_owned()).collect()];
    for report in reports {
        for (series, agreement) in report.series.iter().zip(&report.agreements) {
            let alone = report.series.iter().filter(|s| !s.runs.is_empty()).count() < 2;
            let agree = match agreement {
                Agreement::Agree if alone => "n/a".to_owned(),
                Agreement::Agree => "yes".to_owned(),
                Agreement::Disagree(_) => "NO".to_owned(),
                Agreement::NotComparable(_) => "n/a".to_owned(),
            };
            let kind = format!(
                "{}/{}",
                series.engine.category().name(),
                match series.engine.storage() {
                    Storage::File => "file",
                    Storage::Memory => "memory",
                }
            );
            let mut line = vec![
                report.name.to_owned(),
                series.engine.name().to_owned(),
                kind,
            ];
            match (
                Stats::of(&samples(series)),
                &series.unsupported,
                &series.unavailable,
                &series.failure,
            ) {
                (Some(stats), _, _, _) => {
                    let ops = series.runs.first().map_or(0, |run| run.ops);
                    let rows = series.runs.first().map_or(0, |run| run.rows);
                    line.push(duration_text(stats.median));
                    line.push(format!(
                        "{}..{}",
                        duration_text(stats.min),
                        duration_text(stats.max)
                    ));
                    line.push(
                        per_second(ops, stats.median)
                            .map_or("-".to_owned(), |rate| format!("{rate:.0}")),
                    );
                    line.push(rows.to_string());
                    line.push(errors(series).to_string());
                    line.push(agree.clone());
                    line.push(recall(series).map_or("-".to_owned(), |value| format!("{value:.3}")));
                    line.push(notes(report.name, series));
                }
                (None, Some(reason), _, _) => {
                    line.extend(["-", "-", "-", "-", "-", "n/a", "-"].map(str::to_owned));
                    line.push(format!("unsupported: {}", first_line(reason)));
                }
                (None, None, Some(reason), _) => {
                    line.extend(["-", "-", "-", "-", "-", "n/a", "-"].map(str::to_owned));
                    line.push(format!("UNAVAILABLE: {}", first_line(reason)));
                }
                (None, None, None, failure) => {
                    line.extend(["-", "-", "-", "-", "1", "n/a", "-"].map(str::to_owned));
                    line.push(format!(
                        "failed: {}",
                        first_line(&failure.clone().unwrap_or_default())
                    ));
                }
            }
            lines.push(line);
        }
    }
    let widths: Vec<usize> = (0..header.len())
        .map(|column| {
            lines
                .iter()
                .filter_map(|line| line.get(column))
                .map(|cell| cell.chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    let mut text = String::new();
    for line in engine_lines(reports) {
        text.push_str(&format!("engines, {line}\n"));
    }
    for (index, line) in lines.iter().enumerate() {
        let cells: Vec<String> = line
            .iter()
            .zip(&widths)
            .map(|(cell, width)| format!("{cell:<width$}"))
            .collect();
        text.push_str(cells.join("  ").trim_end());
        text.push('\n');
        if index == 0 {
            text.push_str(&"-".repeat(widths.iter().sum::<usize>() + 2 * (widths.len() - 1)));
            text.push('\n');
        }
    }
    for report in reports {
        for detail in report.disagreements() {
            text.push_str(&format!("DISAGREEMENT in {}: {detail}\n", report.name));
        }
    }
    for (engine, reason, count) in unavailable(reports) {
        text.push_str(&format!(
            "UNAVAILABLE {engine} on {count} workloads: {reason}\n"
        ));
    }
    text
}

/// Every distinct reason an engine could not run, with how many workloads it hit.
pub fn unavailable(reports: &[WorkloadReport]) -> Vec<(&'static str, String, usize)> {
    let mut found: Vec<(&'static str, String, usize)> = Vec::new();
    for series in reports.iter().flat_map(|report| &report.series) {
        let Some(reason) = &series.unavailable else {
            continue;
        };
        match found
            .iter_mut()
            .find(|(engine, known, _)| *engine == series.engine.name() && known == reason)
        {
            Some(entry) => entry.2 += 1,
            None => found.push((series.engine.name(), reason.clone(), 1)),
        }
    }
    found
}

/// The JSON document written to `--out`.
pub fn json(reports: &[WorkloadReport], header: Value) -> Value {
    let rows: Vec<Value> = reports
        .iter()
        .flat_map(|report| {
            report.series.iter().zip(&report.agreements).map(move |(series, agreement)| {
                let stats = Stats::of(&samples(series));
                let first = series.runs.first();
                json!({
                    "workload": report.name,
                    "engine": series.engine.name(),
                    "category": series.engine.category().name(),
                    "storage": series.engine.storage().name(),
                    "status": match (&stats, &series.unsupported, &series.unavailable) {
                        (Some(_), _, _) => "measured",
                        (None, Some(_), _) => "unsupported",
                        (None, None, Some(_)) => "unavailable",
                        (None, None, None) => "failed",
                    },
                    "median_ms": stats.map(|s| s.median.as_secs_f64() * 1e3),
                    "min_ms": stats.map(|s| s.min.as_secs_f64() * 1e3),
                    "max_ms": stats.map(|s| s.max.as_secs_f64() * 1e3),
                    "runs_ms": series.runs.iter().map(|run| run.elapsed.as_secs_f64() * 1e3).collect::<Vec<_>>(),
                    "close_ms": series.runs.iter().map(|run| run.close.as_secs_f64() * 1e3).collect::<Vec<_>>(),
                    "disk_read_bytes": series.runs.iter().map(|run| run.disk_read_bytes).collect::<Vec<_>>(),
                    "ops": first.map(|run| run.ops),
                    "ops_per_second": stats.and_then(|s| first.and_then(|run| per_second(run.ops, s.median))),
                    "rows": first.map(|run| run.rows),
                    "errors": errors(series) + u64::from(stats.is_none() && series.unsupported.is_none() && series.unavailable.is_none()),
                    "bytes": first.and_then(|run| run.bytes),
                    "fingerprint_rows": first.map(|run| run.fingerprint.rows),
                    "fingerprint_checksum": first.map(|run| format!("{:x}", run.fingerprint.checksum)),
                    "recall": recall(series),
                    "note": series.runs.iter().find_map(|run| run.note.clone()),
                    "agree": match agreement {
                        Agreement::Agree => json!(true),
                        Agreement::Disagree(_) => json!(false),
                        Agreement::NotComparable(_) => Value::Null,
                    },
                    "disagreement": match agreement {
                        Agreement::Disagree(detail) => json!(detail),
                        _ => Value::Null,
                    },
                    "unsupported": series.unsupported,
                    "unavailable": series.unavailable,
                    "failure": series.failure.clone().or_else(|| series.runs.iter().find_map(|run| run.failure.clone())),
                })
            })
        })
        .collect();
    let mut document = header;
    document["results"] = Value::Array(rows);
    document
}

#[cfg(test)]
#[path = "report.test.rs"]
mod tests;
