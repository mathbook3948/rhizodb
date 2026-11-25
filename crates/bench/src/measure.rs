use std::time::Duration;
use std::time::Instant;

use crate::engine::EngineError;
use crate::fingerprint::Fingerprint;

/// What one run of one workload on one engine produced.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Measurement {
    /// The time the report shows, closing the database included where the workload closes it.
    pub elapsed: Duration,
    /// The part of `elapsed` spent closing the database, which is zero where nothing closes.
    pub close: Duration,
    /// The operations that succeeded.
    pub ops: u64,
    pub rows: u64,
    pub errors: u64,
    pub fingerprint: Fingerprint,
    pub bytes: Option<u64>,
    pub failure: Option<String>,
    /// How much of the exact result a search found, as the mean recall at 10 over its queries.
    pub recall: Option<f64>,
    /// What an engine did differently that the report must say, such as a scan in place of an
    /// index.
    pub note: Option<String>,
}

/// Counts what a loop of queries did while it ran.
#[derive(Clone, Debug, Default)]
pub struct Tally {
    pub ops: u64,
    pub rows: u64,
    pub errors: u64,
    pub fingerprint: Fingerprint,
    pub failure: Option<String>,
    next: u64,
}

impl Tally {
    pub fn one_error() -> Tally {
        Tally {
            errors: 1,
            ..Tally::default()
        }
    }

    /// Records the result of the next query in sequence.
    pub fn record(&mut self, result: Result<Fingerprint, EngineError>) {
        self.record_at(self.next, result);
    }

    /// Records the result of the query at position `op` of the whole workload. A failure counts
    /// as an error and keeps the first message, and does not count as an operation done.
    pub fn record_at(&mut self, op: u64, result: Result<Fingerprint, EngineError>) {
        self.next = op + 1;
        match result {
            Ok(fingerprint) => {
                self.ops += 1;
                self.rows += fingerprint.rows;
                self.fingerprint = self.fingerprint.merge_at(op, fingerprint);
            }
            Err(error) => {
                self.errors += 1;
                self.failure.get_or_insert_with(|| error.to_string());
            }
        }
    }

    /// Adds the counts of a tally another thread kept.
    pub fn combine(&mut self, other: Tally) {
        self.ops += other.ops;
        self.rows += other.rows;
        self.errors += other.errors;
        self.fingerprint = self.fingerprint.merge(other.fingerprint);
        if self.failure.is_none() {
            self.failure = other.failure;
        }
    }

    /// Turns the tally into a measurement that took `elapsed`.
    pub fn finish(self, elapsed: Duration) -> Measurement {
        Measurement {
            elapsed,
            ops: self.ops,
            rows: self.rows,
            errors: self.errors,
            fingerprint: self.fingerprint,
            failure: self.failure,
            ..Measurement::default()
        }
    }
}

/// Runs `work` and returns how long it took with what it returned.
pub fn timed<T>(work: impl FnOnce() -> T) -> (Duration, T) {
    let start = Instant::now();
    let value = work();
    (start.elapsed(), value)
}

#[cfg(test)]
#[path = "measure.test.rs"]
mod tests;
