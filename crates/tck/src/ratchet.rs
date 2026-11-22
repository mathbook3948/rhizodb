use std::collections::BTreeSet;

use crate::suite::Verdict;

/// The checked-in set of scenarios that must keep passing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ratchet {
    names: BTreeSet<String>,
}

impl Ratchet {
    /// Reads the file format: one trial name per line, blank lines ignored.
    pub fn parse(text: &str) -> Ratchet {
        Ratchet {
            names: text
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(str::to_string)
                .collect(),
        }
    }

    /// Builds a ratchet from the names of passing scenarios.
    pub fn from_passing(names: impl IntoIterator<Item = String>) -> Ratchet {
        Ratchet {
            names: names.into_iter().collect(),
        }
    }

    /// Writes the file format with names sorted.
    pub fn render(&self) -> String {
        self.names.iter().map(|name| format!("{name}\n")).collect()
    }

    /// Listed names that are not in `existing`.
    pub fn stale(&self, existing: &BTreeSet<&str>) -> Vec<&str> {
        self.names
            .iter()
            .map(String::as_str)
            .filter(|name| !existing.contains(name))
            .collect()
    }

    /// Decides whether a scenario's verdict is acceptable given the list.
    pub fn judge(&self, name: &str, verdict: &Verdict) -> Result<(), String> {
        let listed = self.names.contains(name);
        match verdict {
            Verdict::HarnessError(message) => Err(format!("harness error: {message}")),
            Verdict::Passed if listed => Ok(()),
            Verdict::Passed => {
                Err("passes but is not in passing.txt, rerun with TCK_BLESS=1 to record it".into())
            }
            Verdict::Failed(message) if listed => Err(format!(
                "regression, listed in passing.txt but fails: {message}"
            )),
            Verdict::Failed(_) => Ok(()),
        }
    }

    /// Whether nothing is listed.
    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }
}

#[cfg(test)]
#[path = "ratchet.test.rs"]
mod tests;
