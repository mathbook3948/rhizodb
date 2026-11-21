//! An embedded, schemaless property graph database.
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

#[allow(dead_code, reason = "consumed by the Database API in a later unit")]
mod graph;
mod gql;
#[allow(dead_code, reason = "consumed by the Database API in a later unit")]
mod storage;
#[allow(dead_code, reason = "consumed by the Database API in a later unit")]
mod value;
