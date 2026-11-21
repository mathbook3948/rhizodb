//! An embedded, schemaless property graph database.
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

#[allow(dead_code, reason = "raised by the query executor in a later unit")]
mod error;
pub mod gql;
#[allow(dead_code, reason = "consumed by the Database API in a later unit")]
mod graph;
#[allow(
    dead_code,
    reason = "the interpreter that uses runtime values comes in a later unit"
)]
mod query;
#[allow(dead_code, reason = "built by the query executor in a later unit")]
mod result;
#[allow(dead_code, reason = "consumed by the Database API in a later unit")]
mod storage;
#[allow(dead_code, reason = "consumed by the Database API in a later unit")]
mod value;

pub use error::Error;
pub use error::ErrorKind;
pub use gql::error::SyntaxErrorKind;
pub use graph::NodeId;
pub use graph::RelationshipId;
pub use result::Node;
pub use result::Parameters;
pub use result::Path;
pub use result::QueryResult;
pub use result::Relationship;
pub use result::Value;
