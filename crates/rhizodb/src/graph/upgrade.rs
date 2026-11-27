use super::GraphError;
use crate::storage::StorageError;

#[cfg(not(test))]
pub(crate) const UPGRADE_BATCH_BYTES: usize = 16 << 20;
#[cfg(test)]
pub(crate) const UPGRADE_BATCH_BYTES: usize = 64 << 10;

/// How many bytes one commit of an upgrade may still write.
#[derive(Debug)]
pub(crate) struct UpgradeBudget {
    spent: usize,
    limit: usize,
}

impl UpgradeBudget {
    pub(crate) fn new(limit: usize) -> Self {
        Self { spent: 0, limit }
    }

    /// Counts `bytes` that a step wrote or holds for the commit.
    pub(crate) fn spend(&mut self, bytes: usize) {
        self.spent = self.spent.saturating_add(bytes);
    }

    /// Whether the step should end its commit after the unit it is in.
    pub(crate) fn full(&self) -> bool {
        self.spent >= self.limit
    }
}

/// A step of an upgrade, run in commits of bounded size from a cursor the step chooses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UpgradeStep {
    RelationshipChunks,
    PostingBlocks,
    Tokenizer,
    RelationshipChecksums,
}

impl UpgradeStep {
    pub(crate) fn code(self) -> u8 {
        match self {
            Self::RelationshipChunks => 1,
            Self::PostingBlocks => 2,
            Self::Tokenizer => 3,
            Self::RelationshipChecksums => 4,
        }
    }

    pub(crate) fn of_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::RelationshipChunks),
            2 => Some(Self::PostingBlocks),
            3 => Some(Self::Tokenizer),
            4 => Some(Self::RelationshipChecksums),
            _ => None,
        }
    }
}

/// Why a scan of an upgrade step stopped before the end of its range.
pub(crate) enum Scan {
    Full,
    Failed(GraphError),
}

impl From<StorageError> for Scan {
    fn from(error: StorageError) -> Self {
        Self::Failed(GraphError::from(error))
    }
}

impl From<GraphError> for Scan {
    fn from(error: GraphError) -> Self {
        Self::Failed(error)
    }
}

impl From<crate::value::DecodeError> for Scan {
    fn from(error: crate::value::DecodeError) -> Self {
        Self::Failed(GraphError::from(error))
    }
}

/// The outcome of a scan that a step may stop once its commit is full.
pub(crate) fn finished(scanned: Result<(), Scan>) -> Result<bool, GraphError> {
    match scanned {
        Ok(()) => Ok(true),
        Err(Scan::Full) => Ok(false),
        Err(Scan::Failed(error)) => Err(error),
    }
}
