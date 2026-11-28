use std::fs;

use serde_json::Value;

use crate::engine::EngineKind;
use crate::readme::Language;
use crate::readme::ROWS;
use crate::readme::SIZE_FROM;
use crate::readme::Timing;
use crate::readme::cell;
use crate::readme::english_heading;
use crate::readme::mebibytes;
use crate::readme::result;
use crate::readme::splice_between;

/// The line in ENGINES.md after which the generated table starts.
pub const START: &str = "<!-- engines-table:start -->";

/// The line in ENGINES.md before which the generated table ends.
pub const END: &str = "<!-- engines-table:end -->";

fn engines(document: &Value) -> Vec<EngineKind> {
    EngineKind::ALL
        .into_iter()
        .filter(|kind| {
            document["results"].as_array().is_some_and(|rows| {
                rows.iter()
                    .any(|row| row["engine"].as_str() == Some(kind.name()))
            })
        })
        .collect()
}

fn fastest(timings: &[(EngineKind, Option<Timing>)]) -> String {
    let mut measured: Vec<(EngineKind, Timing)> = timings
        .iter()
        .filter_map(|(kind, timing)| timing.map(|timing| (*kind, timing)))
        .filter(|(_, timing)| timing.median > 0.0)
        .collect();
    measured.sort_by(|left, right| left.1.median.total_cmp(&right.1.median));
    match measured.as_slice() {
        [(first, _)] => first.title().to_owned(),
        [(_, best), (_, next), ..] if best.max >= next.min => "similar".to_owned(),
        [(first, best), (_, next), ..] => {
            format!("{} {:.1}x", first.title(), next.median / best.median)
        }
        [] => "n/a".to_owned(),
    }
}

fn smallest(sizes: &[(EngineKind, Option<f64>)]) -> String {
    let mut measured: Vec<(EngineKind, f64)> = sizes
        .iter()
        .filter_map(|(kind, bytes)| bytes.map(|bytes| (*kind, bytes)))
        .filter(|(_, bytes)| *bytes > 0.0)
        .collect();
    measured.sort_by(|left, right| left.1.total_cmp(&right.1));
    match measured.as_slice() {
        [(first, _)] => first.title().to_owned(),
        [(first, least), (_, next), ..] => {
            let ratio = next / least;
            if format!("{ratio:.1}") == "1.0" {
                "similar".to_owned()
            } else {
                format!("{} {ratio:.1}x smaller", first.title())
            }
        }
        [] => "n/a".to_owned(),
    }
}

/// Renders a table of every engine the JSON report holds, one column per engine and a last
/// column that names the fastest and how much faster it is than the next, naming the machine
/// and the date of the run. A workload no engine ran has no row.
pub fn render(document: &Value, machine: &str, date: &str) -> Result<String, String> {
    if !document["results"].is_array() {
        return Err("the report has no results".to_owned());
    }
    let engines = engines(document);
    let mut text = english_heading(document, machine, date);
    text.push_str("| Workload |");
    for kind in &engines {
        text.push_str(&format!(" {} |", kind.title()));
    }
    text.push_str(" Fastest |\n| --- |");
    text.push_str(&" ---: |".repeat(engines.len()));
    text.push_str(" --- |\n");
    for (workload, label) in ROWS {
        if engines
            .iter()
            .all(|kind| result(document, workload, kind.name()).is_none())
        {
            continue;
        }
        text.push_str(&format!("| {label} |"));
        let mut timings = Vec::new();
        for kind in &engines {
            let (shown, timing) = cell(result(document, workload, kind.name()), Language::English);
            text.push_str(&format!(" {shown} |"));
            timings.push((*kind, timing));
        }
        text.push_str(&format!(" {} |\n", fastest(&timings)));
    }
    text.push_str("| File size after bulk load |");
    let mut sizes = Vec::new();
    for kind in &engines {
        let (shown, bytes) = mebibytes(result(document, SIZE_FROM, kind.name()), Language::English);
        text.push_str(&format!(" {shown} |"));
        sizes.push((*kind, bytes));
    }
    text.push_str(&format!(" {} |\n", smallest(&sizes)));
    Ok(text)
}

/// Reads the report at the first argument and rewrites the table in the document at the second,
/// naming the machine of the third argument and the date of the fourth.
pub fn main(arguments: impl IntoIterator<Item = String>) -> Result<(), String> {
    let arguments: Vec<String> = arguments.into_iter().collect();
    let [report, document, machine, date] = arguments.as_slice() else {
        return Err("usage: engines-table REPORT.json ENGINES.md MACHINE DATE".to_owned());
    };
    let report_text =
        fs::read_to_string(report).map_err(|error| format!("cannot read {report}: {error}"))?;
    let parsed: Value = serde_json::from_str(&report_text)
        .map_err(|error| format!("{report} is not a benchmark report: {error}"))?;
    let document_text =
        fs::read_to_string(document).map_err(|error| format!("cannot read {document}: {error}"))?;
    let table = render(&parsed, machine, date)?;
    let updated = splice_between(&document_text, &table, START, END)?;
    fs::write(document, updated).map_err(|error| format!("cannot write {document}: {error}"))
}

#[cfg(test)]
#[path = "engines_table.test.rs"]
mod tests;
