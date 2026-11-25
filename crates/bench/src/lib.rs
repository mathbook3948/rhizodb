//! Measures RhizoDB against other databases on the same graph workloads and compares the numbers.
//!
//! A seeded generator builds one graph that every engine loads. Every workload runs on every
//! engine from the same starting state, one warm-up run is dropped, and the repetitions alternate
//! which engine goes first. Each run walks every row it gets back and fingerprints them, so the
//! report can show whether the engines returned the same result.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

pub mod absent;
pub mod bolt;
pub mod cli;
#[cfg(feature = "cozo")]
pub mod cozo;
pub mod data;
#[cfg(feature = "duckdb")]
pub mod duckdb;
pub mod engine;
pub mod falkor;
pub mod fingerprint;
pub mod ladybug;
pub mod measure;
pub mod random;
pub mod readme;
pub mod report;
pub mod rhizo;
pub mod runner;
pub mod scale;
pub mod sqlite;
pub mod workloads;
