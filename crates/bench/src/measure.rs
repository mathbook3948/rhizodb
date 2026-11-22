use std::time::Duration;
use std::time::Instant;

use crate::engine::EngineError;
use crate::fingerprint::Fingerprint;

/// What one run of one workload on one engine produced.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Measurement {
    pub elapsed: Duration,
    pub ops: u64,
    pub rows: u64,
    pub errors: u64,
    pub fingerprint: Fingerprint,
    pub bytes: Option<u64>,
    pub failure: Option<String>,
}

/// Counts what a loop of queries did while it ran.
#[derive(Clone, Debug, Default)]
pub struct Tally {
    pub ops: u64,
    pub rows: u64,
    pub errors: u64,
    pub fingerprint: Fingerprint,
    pub failure: Option<String>,
}

impl Tally {
    /// Records one query's result. A failure counts as an error and keeps the first message.
    pub fn record(&mut self, result: Result<Fingerprint, EngineError>) {
        self.ops += 1;
        match result {
            Ok(fingerprint) => {
                self.rows += fingerprint.rows;
                self.fingerprint = self.fingerprint.merge(fingerprint);
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
            bytes: None,
            failure: self.failure,
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
