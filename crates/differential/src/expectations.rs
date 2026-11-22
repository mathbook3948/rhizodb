use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;

use crate::compare::Outcome;

/// The note a blessed run writes for a new difference, which a person must replace with the
/// reason the engines differ.
pub const UNEXPLAINED: &str = "UNEXPLAINED";

const NOTE_LIMIT: usize = 160;

/// One listed query: the outcome it must keep, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// An [`Outcome::kind`] other than `agree`.
    pub kind: String,
    /// For `differ`, the documented reason. For the other kinds, the error that was raised.
    pub note: String,
}

/// The checked-in list of queries on which the engines do not agree. Every other query must
/// agree.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Expectations {
    entries: BTreeMap<String, Entry>,
}

/// A line of the expectations file that is not `kind<TAB>name<TAB>note`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExpectationsError {
    /// One-based line number.
    pub line: usize,
    /// What is wrong with it.
    pub message: String,
}

impl fmt::Display for ExpectationsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "line {}: {}", self.line, self.message)
    }
}

impl Error for ExpectationsError {}

const KINDS: [&str; 4] = ["differ", "ladybug-rejects", "rhizo-rejects", "both-reject"];

impl Expectations {
    /// Reads the file format: one `kind<TAB>name<TAB>note` per line, blank lines ignored.
    pub fn parse(text: &str) -> Result<Expectations, ExpectationsError> {
        let mut entries = BTreeMap::new();
        for (index, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let error = |message: &str| ExpectationsError {
                line: index + 1,
                message: message.to_string(),
            };
            let mut fields = line.splitn(3, '\t');
            let (Some(kind), Some(name), Some(note)) =
                (fields.next(), fields.next(), fields.next())
            else {
                return Err(error("expected kind, name, and note separated by tabs"));
            };
            if !KINDS.contains(&kind) {
                return Err(error(&format!("unknown kind {kind}")));
            }
            let entry = Entry {
                kind: kind.to_string(),
                note: note.to_string(),
            };
            if entries.insert(name.to_string(), entry).is_some() {
                return Err(error(&format!("{name} is listed twice")));
            }
        }
        Ok(Expectations { entries })
    }

    /// Writes the file format, sorted by name.
    pub fn render(&self) -> String {
        self.entries
            .iter()
            .map(|(name, entry)| format!("{}\t{name}\t{}\n", entry.kind, entry.note))
            .collect()
    }

    /// Listed names that are not in `existing`.
    pub fn stale(&self, existing: &BTreeSet<&str>) -> Vec<&str> {
        self.entries
            .keys()
            .map(String::as_str)
            .filter(|name| !existing.contains(name))
            .collect()
    }

    /// Decides whether a query's outcome is acceptable given the list.
    pub fn judge(&self, name: &str, outcome: &Outcome) -> Result<(), String> {
        let detail = outcome.detail();
        match (self.entries.get(name), outcome) {
            (None, Outcome::Agree) => Ok(()),
            (None, _) => Err(format!(
                "{} but not listed in differences.txt, rerun with DIFFERENTIAL_BLESS=1: {detail}",
                outcome.kind()
            )),
            (Some(entry), _) if entry.kind != outcome.kind() => Err(format!(
                "listed in differences.txt as {}, but now {}: {detail}",
                entry.kind,
                outcome.kind()
            )),
            (Some(entry), Outcome::Differ(_)) if entry.note == UNEXPLAINED => Err(format!(
                "differs, and differences.txt does not say why: {detail}"
            )),
            (Some(_), _) => Ok(()),
        }
    }

    /// The list a run with these outcomes should check in. Reasons written for a difference
    /// are kept while the query still differs.
    pub fn bless<'a>(&self, outcomes: impl IntoIterator<Item = (&'a str, &'a Outcome)>) -> Self {
        let entries = outcomes
            .into_iter()
            .filter(|(_, outcome)| **outcome != Outcome::Agree)
            .map(|(name, outcome)| {
                let note = match (self.entries.get(name), outcome) {
                    (Some(entry), Outcome::Differ(_)) if entry.kind == "differ" => {
                        entry.note.clone()
                    }
                    (_, Outcome::Differ(_)) => UNEXPLAINED.to_string(),
                    (_, outcome) => outcome.detail().chars().take(NOTE_LIMIT).collect(),
                };
                let entry = Entry {
                    kind: outcome.kind().to_string(),
                    note,
                };
                (name.to_string(), entry)
            })
            .collect();
        Expectations { entries }
    }
}

#[cfg(test)]
#[path = "expectations.test.rs"]
mod tests;
