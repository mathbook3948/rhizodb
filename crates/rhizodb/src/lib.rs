#![doc = include_str!("guide.md")]
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![deny(missing_docs)]

mod database;
mod dump;
mod engine;
mod error;
mod gql;
mod graph;
mod procedure;
mod query;
mod replication;
mod result;
mod snapshot;
mod storage;
mod temporal;
#[cfg(test)]
mod test_scale;
mod transaction;
mod value;

pub use database::Database;
pub use database::DatabaseStats;
pub use database::OpenOptions;
pub use error::Error;
pub use error::ErrorKind;
pub use gql::error::SyntaxErrorKind;
#[cfg(fuzzing)]
#[doc(hidden)]
pub use gql::parse_for_fuzzing;
pub use graph::IndexTarget;
pub use graph::NodeId;
pub use graph::RelationshipId;
pub use graph::VectorMetric;
pub use procedure::Procedure;
pub use procedure::ProcedureError;
pub use procedure::ProcedureOutput;
pub use procedure::ValueType;
pub use replication::Changes;
pub use replication::History;
pub use replication::HistoryEntry;
pub use replication::Lineage;
pub use replication::Position;
pub use replication::ReplicationFailure;
pub use replication::Role;
pub use result::Node;
pub use result::Parameters;
pub use result::Path;
pub use result::QueryResult;
pub use result::Relationship;
pub use result::Value;
pub use snapshot::Snapshot;
pub use temporal::Date;
pub use temporal::DateTime;
pub use temporal::Duration;
pub use temporal::LocalDateTime;
pub use temporal::LocalTime;
pub use temporal::Temporal;
pub use temporal::Time;
pub use transaction::ReadTransaction;
pub use transaction::WriteTransaction;
#[cfg(fuzzing)]
#[doc(hidden)]
pub use value::read_value_for_fuzzing;

/// The README, whose example runs as a doctest.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
pub struct Readme;

/// Runs the storage engine's decoders on `bytes` for the fuzzer, and fails when a decoded value
/// encodes to other bytes.
#[cfg(fuzzing)]
#[doc(hidden)]
pub fn decode_engine_formats_for_fuzzing(bytes: &[u8]) -> Result<(), String> {
    engine::fuzz::decode_formats(bytes)
}

/// Damages a database file of RhizoDB's own engine by `bytes`, opens it, and reads every entry,
/// for the fuzzer. Fails when the damage changed what reads return without an error.
#[cfg(fuzzing)]
#[doc(hidden)]
pub fn open_damaged_engine_file_for_fuzzing(bytes: &[u8]) -> Result<(), String> {
    engine::fuzz::open_damaged_file(bytes)
}

/// Damages a snapshot stream by `bytes`, seals it again, installs it, and reads every entry,
/// for the fuzzer. Fails when the damage changed what reads return without an error.
#[cfg(fuzzing)]
#[doc(hidden)]
pub fn install_damaged_snapshot_for_fuzzing(bytes: &[u8]) -> Result<(), String> {
    engine::fuzz::install_damaged_stream(bytes)
}

/// Damages a batch of replication changes by `bytes`, reseals it, decodes it, applies it to a
/// replica, and reads the replica, for the fuzzer. Fails when decoded changes keep other bytes.
#[cfg(fuzzing)]
#[doc(hidden)]
pub fn apply_changes_for_fuzzing(bytes: &[u8]) -> Result<(), String> {
    replication::apply_changes_for_fuzzing(bytes)
}
