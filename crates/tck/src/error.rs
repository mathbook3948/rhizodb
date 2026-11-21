use std::error::Error;
use std::fmt;

/// The harness itself could not handle something in the TCK, as opposed to the engine failing it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HarnessError {
    /// What went wrong and where.
    pub message: String,
}

impl fmt::Display for HarnessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for HarnessError {}
