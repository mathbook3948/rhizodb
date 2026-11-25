use std::fs;

use serde_json::Value;

/// The line in README.md after which the generated table starts.
pub const START: &str = "<!-- bench-table:start -->";

/// The line in README.md before which the generated table ends.
pub const END: &str = "<!-- bench-table:end -->";

/// The engine every row compares with RhizoDB.
pub const OTHER: &str = "ladybugdb";

/// The workloads README.md shows, in order, each with the label of its row.
pub const ROWS: [(&str, &str); 14] = [
    ("read/point-lookup", "Point lookup by id"),
    ("read/filter-scan", "Filtered scan"),
    ("read/order-limit", "`ORDER BY ... LIMIT`"),
    ("traverse/1-hop", "1-hop neighbours"),
    ("traverse/2-hop-distinct", "2-hop distinct"),
    ("traverse/3-hop-distinct", "3-hop distinct"),
    ("traverse/variable-length-1..3", "Variable length `*1..3`"),
    ("aggregate/group-by-city", "Group by city"),
    ("index/keyword-search", "Keyword search"),
    ("index/vector-search", "Vector search"),
    ("write/small-commits", "Small commits"),
    ("write/bulk-update", "Bulk update"),
    ("write/detach-delete", "Detach delete"),
    ("load/bulk", "Bulk load"),
];

/// The workload whose on-disk size the size row shows.
pub const SIZE_FROM: &str = "load/bulk";

fn result<'a>(document: &'a Value, workload: &str, engine: &str) -> Option<&'a Value> {
    document["results"].as_array()?.iter().find(|row| {
        row["workload"].as_str() == Some(workload) && row["engine"].as_str() == Some(engine)
    })
}

fn milliseconds(value: f64) -> String {
    if value >= 100.0 {
        format!("{value:.0}")
    } else if value >= 10.0 {
        format!("{value:.1}")
    } else if value >= 1.0 {
        format!("{value:.2}")
    } else {
        format!("{value:.3}")
    }
}

fn cell(row: Option<&Value>) -> (String, Option<f64>) {
    let Some(row) = row else {
        return ("not run".to_owned(), None);
    };
    match row["median_ms"].as_f64() {
        Some(median) if row["status"].as_str() == Some("measured") => {
            (milliseconds(median), Some(median))
        }
        _ => (row["status"].as_str().unwrap_or("failed").to_owned(), None),
    }
}

fn faster(rhizodb: Option<f64>, other: Option<f64>) -> String {
    match (rhizodb, other) {
        (Some(ours), Some(theirs)) if ours > 0.0 && theirs > 0.0 => {
            if ours <= theirs {
                format!("RhizoDB {:.1}x", theirs / ours)
            } else {
                format!("LadybugDB {:.1}x", ours / theirs)
            }
        }
        _ => "n/a".to_owned(),
    }
}

fn mebibytes(row: Option<&Value>) -> (String, Option<f64>) {
    match row.and_then(|row| row["bytes"].as_f64()) {
        Some(bytes) => (format!("{:.1} MiB", bytes / 1_048_576.0), Some(bytes)),
        None => ("n/a".to_owned(), None),
    }
}

fn smaller(rhizodb: Option<f64>, other: Option<f64>) -> String {
    match (rhizodb, other) {
        (Some(ours), Some(theirs)) if ours > 0.0 && theirs > 0.0 => {
            if ours <= theirs {
                format!("RhizoDB {:.1}x smaller", theirs / ours)
            } else {
                format!("LadybugDB {:.1}x smaller", ours / theirs)
            }
        }
        _ => "n/a".to_owned(),
    }
}

/// Renders the README table from the JSON report the benchmark binary writes with `--out`,
/// naming the machine and the date of the run.
pub fn render(document: &Value, machine: &str, date: &str) -> Result<String, String> {
    if !document["results"].is_array() {
        return Err("the report has no results".to_owned());
    }
    let scale = document["scale"].as_str().unwrap_or("unknown");
    let reps = document["reps"].as_u64().unwrap_or(0);
    let persons = document["persons"].as_u64().unwrap_or(0);
    let knows = document["knows"].as_u64().unwrap_or(0);
    let cities = document["cities"].as_u64().unwrap_or(0);
    let mut text = format!(
        "Measured on {date} on {machine}, at the `{scale}` scale ({persons} persons, {knows} `KNOWS` edges, {cities} cities), with {reps} repetitions per workload. Times are medians in milliseconds for the whole workload, lower is better.\n\n"
    );
    text.push_str("| Workload | RhizoDB | LadybugDB | Faster |\n");
    text.push_str("| --- | ---: | ---: | --- |\n");
    for (workload, label) in ROWS {
        let (ours, ours_ms) = cell(result(document, workload, "rhizodb"));
        let (theirs, theirs_ms) = cell(result(document, workload, OTHER));
        let winner = faster(ours_ms, theirs_ms);
        text.push_str(&format!("| {label} | {ours} | {theirs} | {winner} |\n"));
    }
    let (ours, ours_bytes) = mebibytes(result(document, SIZE_FROM, "rhizodb"));
    let (theirs, theirs_bytes) = mebibytes(result(document, SIZE_FROM, OTHER));
    let winner = smaller(ours_bytes, theirs_bytes);
    text.push_str(&format!(
        "| File size after bulk load | {ours} | {theirs} | {winner} |\n"
    ));
    Ok(text)
}

/// Replaces the text between [`START`] and [`END`] in `readme` with `table`.
pub fn splice(readme: &str, table: &str) -> Result<String, String> {
    let start = readme
        .find(START)
        .ok_or_else(|| format!("README.md has no `{START}` line"))?;
    let end = readme
        .find(END)
        .filter(|end| *end > start)
        .ok_or_else(|| format!("README.md has no `{END}` line after `{START}`"))?;
    Ok(format!(
        "{}{START}\n{table}{}",
        &readme[..start],
        &readme[end..]
    ))
}

/// Reads the report at the first argument and rewrites the table in the README at the second,
/// naming the machine of the third argument and the date of the fourth.
pub fn main(arguments: impl IntoIterator<Item = String>) -> Result<(), String> {
    let arguments: Vec<String> = arguments.into_iter().collect();
    let [report, readme, machine, date] = arguments.as_slice() else {
        return Err("usage: readme-table REPORT.json README.md MACHINE DATE".to_owned());
    };
    let report_text =
        fs::read_to_string(report).map_err(|error| format!("cannot read {report}: {error}"))?;
    let document: Value = serde_json::from_str(&report_text)
        .map_err(|error| format!("{report} is not a benchmark report: {error}"))?;
    let readme_text =
        fs::read_to_string(readme).map_err(|error| format!("cannot read {readme}: {error}"))?;
    let table = render(&document, machine, date)?;
    let updated = splice(&readme_text, &table)?;
    fs::write(readme, updated).map_err(|error| format!("cannot write {readme}: {error}"))
}

#[cfg(test)]
#[path = "readme.test.rs"]
mod tests;
