use std::error::Error;
use std::fmt;
use std::io;

use crate::storage::StorageError;

/// A failure of the engine. Corruption and an unsupported format are told apart from I/O so the
/// storage layer can report damaged data as such.
#[derive(Debug)]
pub(crate) enum EngineError {
    Io(io::Error),
    /// Stored bytes fail a checksum or do not decode.
    Corrupt(String),
    /// The file or log was written by a newer engine format, or uses a feature this build lacks.
    Unsupported(String),
    /// The header says the log holds commits, but no log segment is there.
    WalMissing,
    /// A key, value, or transaction exceeds what the engine can store.
    TooLarge,
}

impl EngineError {
    pub(crate) fn corrupt(message: impl Into<String>) -> Self {
        Self::Corrupt(message.into())
    }

    pub(crate) fn is_corrupt(&self) -> bool {
        matches!(self, Self::Corrupt(_) | Self::WalMissing)
    }
}

impl fmt::Display for EngineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "I/O error: {error}"),
            Self::Corrupt(message) => write!(formatter, "corrupt database: {message}"),
            Self::Unsupported(message) => write!(formatter, "unsupported database: {message}"),
            Self::WalMissing => formatter.write_str(
                "write-ahead log missing: the database was not closed cleanly and its log segments \
                 are gone",
            ),
            Self::TooLarge => formatter.write_str("a key, value, or transaction is too large"),
        }
    }
}

impl Error for EngineError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for EngineError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<EngineError> for StorageError {
    fn from(error: EngineError) -> Self {
        if error.is_corrupt() {
            StorageError::corrupt(error)
        } else {
            StorageError::new(error)
        }
    }
}
