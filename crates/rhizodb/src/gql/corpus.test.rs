use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

use super::parser::parse;

const VENDORED_FEATURES: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../tck/vendor/opencypher/features"
);

const QUERY_STEPS: [&str; 3] = [
    "When executing query:",
    "And having executed:",
    "When executing control query:",
];

const PARSE_LEVEL_ERRORS: [&str; 6] = [
    "UnexpectedSyntax",
    "InvalidNumberLiteral",
    "InvalidUnicodeLiteral",
    "InvalidUnicodeCharacter",
    "IntegerOverflow",
    "FloatingPointOverflow",
];

struct CorpusQuery {
    location: String,
    text: String,
    expected_error: Option<String>,
}

fn feature_files(directory: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            feature_files(&path, found);
        } else if path.extension().is_some_and(|ext| ext == "feature") {
            found.push(path);
        }
    }
    found.sort();
}

fn table_rows(lines: &[&str]) -> Vec<BTreeMap<String, String>> {
    let Some(examples) = lines
        .iter()
        .position(|line| line.trim_start().starts_with("Examples:"))
    else {
        return Vec::new();
    };
    let mut rows = lines[examples + 1..]
        .iter()
        .map(|line| line.trim())
        .filter(|line| line.starts_with('|'))
        .map(|line| {
            line.trim_matches('|')
                .split('|')
                .map(|cell| cell.trim().to_owned())
                .collect::<Vec<_>>()
        });
    let Some(header) = rows.next() else {
        return Vec::new();
    };
    rows.map(|cells| header.iter().cloned().zip(cells).collect())
        .collect()
}

fn docstrings(lines: &[&str]) -> Vec<(String, String)> {
    let mut found = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        if lines[index].trim() != "\"\"\"" {
            index += 1;
            continue;
        }
        let indent = lines[index].len() - lines[index].trim_start().len();
        let step = lines[..index]
            .iter()
            .rev()
            .map(|line| line.trim())
            .find(|line| !line.is_empty())
            .unwrap_or_default()
            .to_owned();
        let body: Vec<&str> = lines[index + 1..]
            .iter()
            .take_while(|line| line.trim() != "\"\"\"")
            .map(|line| line.get(indent..).unwrap_or(line.trim()))
            .collect();
        index += body.len() + 2;
        found.push((step, body.join("\n")));
    }
    found
}

fn scenario_queries(feature: &str, scenario: &[&str]) -> Vec<CorpusQuery> {
    let name = scenario.first().map_or("", |line| line.trim()).to_owned();
    let expected_error = scenario
        .iter()
        .find(|line| line.contains("should be raised"))
        .and_then(|line| line.rsplit(": ").next())
        .map(|detail| detail.trim().to_owned());
    let rows = table_rows(scenario);
    let bindings = if rows.is_empty() {
        vec![BTreeMap::new()]
    } else {
        rows
    };
    let steps = docstrings(scenario);
    bindings
        .iter()
        .flat_map(|binding| {
            steps
                .iter()
                .filter(|(step, _)| QUERY_STEPS.iter().any(|known| step.ends_with(known)))
                .map(|(step, text)| {
                    let text = binding.iter().fold(text.clone(), |text, (key, value)| {
                        text.replace(&format!("<{key}>"), value)
                    });
                    let is_setup = step.contains("having executed");
                    CorpusQuery {
                        location: format!("{feature} {name}"),
                        text,
                        expected_error: if is_setup {
                            None
                        } else {
                            expected_error.clone()
                        },
                    }
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

fn corpus(root: &Path) -> Vec<CorpusQuery> {
    let mut files = Vec::new();
    feature_files(root, &mut files);
    let mut queries = Vec::new();
    for file in files {
        let Ok(content) = fs::read_to_string(&file) else {
            continue;
        };
        let feature = file
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let lines: Vec<&str> = content.lines().collect();
        let starts: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, line)| line.trim_start().starts_with("Scenario"))
            .map(|(index, _)| index)
            .collect();
        for (position, start) in starts.iter().enumerate() {
            let end = starts.get(position + 1).copied().unwrap_or(lines.len());
            queries.extend(scenario_queries(&feature, &lines[*start..end]));
        }
    }
    queries
}

fn deferred_cause(query: &str) -> Option<&'static str> {
    let lowered = query.to_ascii_lowercase();
    let words: Vec<&str> = lowered
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .collect();
    let has = |word: &str| words.contains(&word);
    if has("merge") {
        Some("MERGE clause")
    } else if has("foreach") {
        Some("FOREACH clause")
    } else if has("shortestpath") || has("allshortestpaths") {
        Some("shortestPath patterns")
    } else if has("load") {
        Some("LOAD CSV")
    } else {
        None
    }
}

#[test]
fn every_vendored_tck_query_that_should_compile_parses_or_is_deferred() {
    let root = Path::new(VENDORED_FEATURES);
    if !root.is_dir() {
        println!("skipping the TCK corpus test: {VENDORED_FEATURES} is not vendored here");
        return;
    }
    let queries = corpus(root);
    let mut parsed = 0;
    let mut deferred: BTreeMap<&str, usize> = BTreeMap::new();
    let mut unparsed: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut rejected_as_expected = 0;
    let mut accepted_despite_error = Vec::new();
    for query in &queries {
        let result = parse(&query.text);
        match (&query.expected_error, result) {
            (None, Ok(_)) => parsed += 1,
            (None, Err(error)) => match deferred_cause(&query.text) {
                Some(cause) => *deferred.entry(cause).or_default() += 1,
                None => unparsed
                    .entry(format!("{:?}", error.kind))
                    .or_default()
                    .push(format!(
                        "{} at {}: {}",
                        query.location,
                        error.offset,
                        query.text.replace('\n', " ")
                    )),
            },
            (Some(detail), Ok(_)) if PARSE_LEVEL_ERRORS.contains(&detail.as_str()) => {
                accepted_despite_error.push(format!("{detail} {}", query.location));
            }
            (Some(_), Err(_)) => rejected_as_expected += 1,
            (Some(_), Ok(_)) => {}
        }
    }
    let should_parse = queries
        .iter()
        .filter(|q| q.expected_error.is_none())
        .count();
    println!(
        "TCK corpus: {} queries, {should_parse} expected to compile, {parsed} parsed, {} deferred {deferred:?}, {} unparsed",
        queries.len(),
        deferred.values().sum::<usize>(),
        unparsed.values().map(Vec::len).sum::<usize>(),
    );
    println!(
        "expected-error scenarios rejected: {rejected_as_expected}, parse-level errors accepted: {}",
        accepted_despite_error.len()
    );
    for (kind, cases) in &unparsed {
        println!("unparsed ({kind}): {}", cases.len());
        for case in cases {
            println!("  {case}");
        }
    }
    for case in &accepted_despite_error {
        println!("accepted but TCK expects a syntax-level error: {case}");
    }
    assert!(
        unparsed.is_empty(),
        "queries outside the deferred features failed to parse"
    );
    assert!(parsed > 0, "no TCK query was parsed");
}
