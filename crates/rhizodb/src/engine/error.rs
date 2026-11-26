use std::error::Error;
use std::fmt;
use std::io;

use crate::error::ErrorKind;
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
    /// An earlier write or sync failed, so the engine refuses every write until it is reopened.
    Poisoned,
    /// A key, value, or transaction exceeds what the engine can store.
    TooLarge,
    /// A failure that already crossed the storage trait, such as one a scan's visitor raised.
    Storage(StorageError),
    /// Changes or a promotion that replication refuses, reported as `kind`.
    Replication(ErrorKind, String),
}

impl EngineError {
    pub(crate) fn corrupt(message: impl Into<String>) -> Self {
        Self::Corrupt(message.into())
    }

    pub(crate) fn is_corrupt(&self) -> bool {
        match self {
            Self::Corrupt(_) | Self::WalMissing => true,
            Self::Storage(error) => error.is_corrupt(),
            Self::Io(error) => error.kind() == io::ErrorKind::UnexpectedEof,
            _ => false,
        }
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
            Self::Poisoned => formatter.write_str(
                "an earlier write failed, so the database refuses writes until it is opened again",
            ),
            Self::TooLarge => formatter.write_str("a key, value, or transaction is too large"),
            Self::Storage(error) => write!(formatter, "{error}"),
            Self::Replication(_, message) => formatter.write_str(message),
        }
    }
}

impl Error for EngineError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Storage(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for EngineError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<StorageError> for EngineError {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}

impl From<EngineError> for StorageError {
    fn from(error: EngineError) -> Self {
        if let EngineError::Storage(error) = error {
            return error;
        }
        if let EngineError::Replication(kind, _) = &error {
            StorageError::of_kind(*kind, error)
        } else if error.is_corrupt() {
            StorageError::corrupt(error)
        } else if matches!(&error, EngineError::Io(io) if io.kind() == io::ErrorKind::WouldBlock) {
            StorageError::of_kind(ErrorKind::DatabaseAlreadyOpen, error)
        } else {
            StorageError::new(error)
        }
    }
}
