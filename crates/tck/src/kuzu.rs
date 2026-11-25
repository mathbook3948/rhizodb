//! Runs test files vendored from Kuzu (MIT, see `vendor/kuzu/LICENSE`) against RhizoDB.
//!
//! A file holds cases, each a list of statements with the outcome Kuzu expects. Kuzu declares a
//! schema before it writes, and RhizoDB is schemaless, so statements that only declare, alter,
//! or drop a table succeed without running, except that a node table's primary key becomes a
//! property index so lookups by key cost what they cost in Kuzu. Settings such as
//! `CALL auto_checkpoint=false` and `CHECKPOINT` succeed without running too. `BEGIN TRANSACTION`, `COMMIT`, and `ROLLBACK` map
//! onto RhizoDB's transactions, and `-RELOADDB` reopens the database file. An expected error
//! matches any error, since the two engines word their errors differently. Directives this
//! runner does not know, such as several connections, make the case fail with the directive
//! named, so it stays out of `passing.txt`.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;

use libtest_mimic::Arguments;
use libtest_mimic::Failed;
use libtest_mimic::Trial;
use rhizodb::Database;
use rhizodb::IndexTarget;
use rhizodb::Parameters;
use rhizodb::ReadTransaction;
use rhizodb::Value;
use rhizodb::WriteTransaction;

use crate::ratchet::Ratchet;
use crate::suite::Verdict;

/// What Kuzu expects a statement to give.
#[derive(Clone, Debug, PartialEq)]
pub enum Expect {
    /// Success, whatever the rows.
    Ok,
    /// Any error.
    Error,
    /// These rows, each as Kuzu prints it with columns joined by `|`.
    Rows(Vec<String>),
    /// A hash of the rows, which this runner cannot check.
    Hash,
}

/// One step of a case.
#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    /// A statement and its expected outcome, in order when `ordered`.
    Statement {
        query: String,
        expect: Expect,
        ordered: bool,
    },
    /// Close the database and open its file again.
    Reload,
}

/// A case of a Kuzu test file.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Case {
    /// `<file path>::<case name>`.
    pub name: String,
    pub steps: Vec<Step>,
    /// Why the runner cannot run the case, when it cannot.
    pub unsupported: Option<String>,
    /// Whether Kuzu itself skips the case.
    pub skipped: bool,
}

const IGNORED_DIRECTIVES: [&str; 12] = [
    "-LOG",
    "-PARALLELISM",
    "-SKIP_IN_MEM",
    "-SKIP_WASM",
    "-SKIP_NODE_GROUP_SIZE_TESTS",
    "-SKIP_FSM_LEAK_CHECK",
    "-SKIP_SEGMENT_SIZE_TESTS",
    "-SKIP_PAGE_SIZE_TESTS",
    "-SKIP_COMPRESSION_DISABLED",
    "-TEST_FWD_ONLY_REL",
    "-CHECKPOINT_WAIT_TIMEOUT",
    "-BUFFER_POOL_SIZE",
];

fn directive(line: &str) -> &str {
    line.split_whitespace().next().unwrap_or("")
}

/// Parses one test file. `relative` is its path inside the vendored tree, which names its
/// cases.
pub fn parse_file(relative: &str, text: &str) -> Result<Vec<Case>, String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut cases: Vec<Case> = Vec::new();
    let mut ordered = false;
    let mut header = Case::default();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index].trim_end();
        index += 1;
        if line.trim().is_empty() || line.starts_with('#') || line == "--" {
            continue;
        }
        let name = directive(line);
        if name == "-DATASET" {
            if line.trim() != "-DATASET CSV empty" && line.trim() != "-DATASET CSV EMPTY" {
                return Err(format!("{relative}: only the empty dataset is supported"));
            }
            continue;
        }
        if name == "-CASE" {
            let case_name = line["-CASE".len()..].trim();
            cases.push(Case {
                name: format!("{relative}::{case_name}"),
                steps: Vec::new(),
                ..header.clone()
            });
            ordered = false;
            continue;
        }
        let case = match cases.last_mut() {
            Some(case) => case,
            None => &mut header,
        };
        match name {
            "-STATEMENT" => {
                let mut query = line["-STATEMENT".len()..].trim().to_owned();
                while index < lines.len()
                    && !lines[index].starts_with('-')
                    && !lines[index].trim().is_empty()
                {
                    query.push('\n');
                    query.push_str(lines[index].trim());
                    index += 1;
                }
                let header = lines.get(index).map_or("", |line| line.trim_end());
                let Some(outcome) = header.strip_prefix("----") else {
                    case.unsupported.get_or_insert_with(|| {
                        "a statement without an expected outcome".to_owned()
                    });
                    continue;
                };
                index += 1;
                let outcome = outcome.trim();
                let expect = match outcome {
                    "ok" => Expect::Ok,
                    "hash" => {
                        index += 1;
                        Expect::Hash
                    }
                    _ if outcome.starts_with("error") => {
                        while index < lines.len()
                            && !lines[index].starts_with('-')
                            && !lines[index].trim().is_empty()
                        {
                            index += 1;
                        }
                        Expect::Error
                    }
                    count => {
                        let count: usize = count.parse().map_err(|_| {
                            format!("{relative}:{index}: unknown outcome `{header}`")
                        })?;
                        let rows = lines
                            .get(index..index + count)
                            .ok_or_else(|| format!("{relative}:{index}: too few rows"))?;
                        index += count;
                        Expect::Rows(rows.iter().map(|row| (*row).to_owned()).collect())
                    }
                };
                if query.starts_with('[') {
                    case.unsupported
                        .get_or_insert_with(|| "statements on named connections".to_owned());
                }
                case.steps.push(Step::Statement {
                    query,
                    expect,
                    ordered,
                });
                ordered = false;
            }
            "-RELOADDB" => case.steps.push(Step::Reload),
            "-CHECK_ORDER" => ordered = true,
            "-SKIP" => case.skipped = true,
            _ if IGNORED_DIRECTIVES.contains(&name) => {}
            _ => {
                case.unsupported
                    .get_or_insert_with(|| format!("the directive {name}"));
            }
        }
    }
    Ok(cases)
}

/// Loads every `.test` file under `root`.
pub fn load(root: &Path) -> Result<Vec<Case>, String> {
    let mut files = Vec::new();
    collect(root, root, &mut files)?;
    files.sort();
    let mut cases = Vec::new();
    for (relative, path) in files {
        let text = fs::read_to_string(&path).map_err(|error| format!("{relative}: {error}"))?;
        cases.extend(parse_file(&relative, &text)?);
    }
    Ok(cases)
}

fn collect(
    root: &Path,
    directory: &Path,
    files: &mut Vec<(String, std::path::PathBuf)>,
) -> Result<(), String> {
    let entries = fs::read_dir(directory).map_err(|error| format!("{directory:?}: {error}"))?;
    for entry in entries {
        let path = entry.map_err(|error| error.to_string())?.path();
        if path.is_dir() {
            collect(root, &path, files)?;
        } else if path
            .extension()
            .is_some_and(|extension| extension == "test")
        {
            let relative = path
                .strip_prefix(root)
                .map_err(|error| error.to_string())?
                .to_string_lossy()
                .replace('\\', "/");
            files.push((relative, path));
        }
    }
    Ok(())
}

/// A value as Kuzu prints it in a test file.
pub fn kuzu_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Boolean(true) => "True".to_owned(),
        Value::Boolean(false) => "False".to_owned(),
        Value::Integer(value) => value.to_string(),
        Value::Float(value) => format!("{value:.6}"),
        Value::String(value) => value.clone(),
        Value::List(items) => {
            let items: Vec<String> = items.iter().map(kuzu_text).collect();
            format!("[{}]", items.join(","))
        }
        Value::Map(entries) => {
            let entries: Vec<String> = entries
                .iter()
                .map(|(key, value)| format!("{key}: {}", kuzu_text(value)))
                .collect();
            format!("{{{}}}", entries.join(", "))
        }
        Value::Temporal(value) => value.to_string(),
        other => format!("{other:?}"),
    }
}

enum Open<'a> {
    None,
    Read(Box<ReadTransaction<'a>>),
    Write(Box<WriteTransaction<'a>>),
}

enum Control {
    Ddl,
    Setting,
    Begin { read_only: bool },
    Commit,
    Rollback,
    Query,
}

fn control(query: &str) -> Control {
    let words: Vec<String> = query
        .trim()
        .trim_end_matches(';')
        .split_whitespace()
        .take(4)
        .map(str::to_ascii_uppercase)
        .collect();
    let words: Vec<&str> = words.iter().map(String::as_str).collect();
    match words.as_slice() {
        ["CREATE", "NODE", "TABLE", ..]
        | ["CREATE", "REL", "TABLE", ..]
        | ["DROP", "TABLE", ..]
        | ["ALTER", "TABLE", ..] => Control::Ddl,
        ["CHECKPOINT"] => Control::Setting,
        ["CALL", setting, ..] if setting.contains('=') => Control::Setting,
        ["BEGIN", "TRANSACTION", "READ", "ONLY"] => Control::Begin { read_only: true },
        ["BEGIN", "TRANSACTION"] => Control::Begin { read_only: false },
        ["COMMIT"] => Control::Commit,
        ["ROLLBACK"] => Control::Rollback,
        _ => Control::Query,
    }
}

fn primary_key(query: &str) -> Option<(String, String)> {
    let upper = query.to_ascii_uppercase();
    let table_start = upper.find("NODE TABLE")? + "NODE TABLE".len();
    let table: String = query[table_start..]
        .trim_start()
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    let key_start = upper.find("PRIMARY KEY")? + "PRIMARY KEY".len();
    let rest = &query[key_start..];
    let open = rest.find('(')?;
    let close = rest.find(')')?;
    let column = rest.get(open + 1..close)?.trim().to_owned();
    (!table.is_empty() && !column.is_empty()).then_some((table, column))
}

type Outcome = Result<Option<Vec<Vec<Value>>>, String>;

fn run_statement<'a>(database: &'a Database, open: &mut Open<'a>, query: &str) -> Outcome {
    let failed = |error: rhizodb::Error| error.to_string();
    match control(query) {
        Control::Setting => Ok(None),
        Control::Ddl => {
            if let Some((table, column)) = primary_key(query) {
                let _ = database.create_property_index(
                    &format!("kuzu_primary_key_{table}"),
                    IndexTarget::Label(&table),
                    &column,
                );
            }
            Ok(None)
        }
        Control::Begin { read_only } => {
            if !matches!(open, Open::None) {
                return Err("a transaction is already open".to_owned());
            }
            *open = if read_only {
                Open::Read(Box::new(database.begin_read().map_err(failed)?))
            } else {
                Open::Write(Box::new(database.begin_write().map_err(failed)?))
            };
            Ok(None)
        }
        Control::Commit => match std::mem::replace(open, Open::None) {
            Open::None => Err("no transaction is open".to_owned()),
            Open::Read(_) => Ok(None),
            Open::Write(tx) => tx.commit().map(|()| None).map_err(failed),
        },
        Control::Rollback => match std::mem::replace(open, Open::None) {
            Open::None => Err("no transaction is open".to_owned()),
            Open::Read(_) => Ok(None),
            Open::Write(tx) => tx.rollback().map(|()| None).map_err(failed),
        },
        Control::Query => {
            let none = Parameters::new();
            let result = match open {
                Open::None => database.execute(query, &none),
                Open::Read(tx) => tx.execute(query, &none),
                Open::Write(tx) => tx.execute(query, &none),
            };
            result
                .map(|result| Some(result.rows().to_vec()))
                .map_err(failed)
        }
    }
}

fn judge(query: &str, expect: &Expect, ordered: bool, outcome: Outcome) -> Result<(), String> {
    match (expect, outcome) {
        (Expect::Hash, _) => Err(format!("{query}: the expected rows are a hash")),
        (Expect::Ok, Ok(_)) | (Expect::Error, Err(_)) => Ok(()),
        (Expect::Error, Ok(rows)) => Err(format!("{query}: expected an error, got {rows:?}")),
        (_, Err(error)) => Err(format!("{query}: {error}")),
        (Expect::Rows(want), Ok(rows)) => {
            let mut got: Vec<String> = rows
                .unwrap_or_default()
                .iter()
                .map(|row| row.iter().map(kuzu_text).collect::<Vec<_>>().join("|"))
                .collect();
            let mut want = want.clone();
            if !ordered {
                got.sort();
                want.sort();
            }
            if got == want {
                Ok(())
            } else {
                Err(format!("{query}: expected {want:?}, got {got:?}"))
            }
        }
    }
}

fn run_segment(path: &Path, steps: &[Step]) -> Result<(), String> {
    let database = Database::open(path).map_err(|error| format!("open: {error}"))?;
    let mut open = Open::None;
    for step in steps {
        if let Step::Statement {
            query,
            expect,
            ordered,
        } = step
        {
            let outcome = run_statement(&database, &mut open, query);
            judge(query, expect, *ordered, outcome)?;
        }
    }
    Ok(())
}

/// Runs `case` on a fresh database file.
pub fn evaluate(case: &Case) -> Verdict {
    if let Some(reason) = &case.unsupported {
        return Verdict::Failed(format!("the runner does not support {reason}"));
    }
    let directory = match tempfile::tempdir() {
        Ok(directory) => directory,
        Err(error) => return Verdict::HarnessError(error.to_string()),
    };
    let path = directory.path().join("kuzu.rhizodb");
    for segment in case.steps.split(|step| *step == Step::Reload) {
        if let Err(message) = run_segment(&path, segment) {
            return Verdict::Failed(message);
        }
    }
    Verdict::Passed
}

/// Runs every vendored case as a libtest-style binary, judged by the ratchet at
/// `ratchet_path`. With `TCK_BLESS=1` the ratchet is rewritten with the cases that pass, and
/// with `KUZU_REPORT=1` every failing case prints why it fails.
pub fn main(root: &Path, ratchet_path: &Path) -> ExitCode {
    let args = Arguments::from_args();
    let bless = std::env::var("TCK_BLESS").is_ok_and(|value| value == "1");
    let report = std::env::var("KUZU_REPORT").is_ok_and(|value| value == "1");
    if bless && (args.filter.is_some() || !args.skip.is_empty() || args.exact) {
        eprintln!("kuzu: refusing to bless a partial run");
        return ExitCode::FAILURE;
    }
    let cases = match load(root) {
        Ok(cases) => cases,
        Err(error) => {
            eprintln!("kuzu: {error}");
            return ExitCode::FAILURE;
        }
    };
    let ratchet = match fs::read_to_string(ratchet_path) {
        Ok(text) => Ratchet::parse(&text),
        Err(error) => {
            eprintln!("kuzu: cannot read {}: {error}", ratchet_path.display());
            return ExitCode::FAILURE;
        }
    };
    let existing: BTreeSet<&str> = cases.iter().map(|case| case.name.as_str()).collect();
    let stale = ratchet.stale(&existing);
    if !bless && !stale.is_empty() {
        for name in stale {
            eprintln!("kuzu: passing.txt lists a case that does not exist: {name}");
        }
        return ExitCode::FAILURE;
    }
    let total = cases.len();
    let ratchet = Arc::new(ratchet);
    let passing = Arc::new(Mutex::new(BTreeSet::new()));
    let trials: Vec<Trial> = cases
        .into_iter()
        .map(|case| {
            let ratchet = Arc::clone(&ratchet);
            let passing = Arc::clone(&passing);
            let skipped = case.skipped;
            Trial::test(case.name.clone(), move || {
                let verdict = evaluate(&case);
                if report && let Verdict::Failed(message) = &verdict {
                    eprintln!("kuzu: {} fails: {message}", case.name);
                }
                if verdict == Verdict::Passed {
                    passing
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .insert(case.name.clone());
                }
                if bless {
                    return match verdict {
                        Verdict::HarnessError(message) => Err(Failed::from(message)),
                        Verdict::Passed | Verdict::Failed(_) => Ok(()),
                    };
                }
                ratchet.judge(&case.name, &verdict).map_err(Failed::from)
            })
            .with_ignored_flag(skipped)
        })
        .collect();
    let conclusion = libtest_mimic::run(&args, trials);
    let passing = passing
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    println!("kuzu: {}/{total} cases passing", passing.len());
    if !bless {
        return conclusion.exit_code();
    }
    if conclusion.has_failed() {
        eprintln!("kuzu: not writing passing.txt because of harness errors");
        return ExitCode::FAILURE;
    }
    match fs::write(ratchet_path, Ratchet::from_passing(passing).render()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("kuzu: cannot write {}: {error}", ratchet_path.display());
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
#[path = "kuzu.test.rs"]
mod tests;
