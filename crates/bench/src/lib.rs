//! Measures RhizoDB and LadybugDB on the same graph workloads and compares the numbers.
//!
//! A seeded generator builds one graph that both engines load into file-backed databases. Every
//! workload runs on both engines from the same starting state, one warm-up run is dropped, and
//! the repetitions alternate which engine goes first. Each run walks every row it gets back and
//! fingerprints them, so the report can show whether the two engines returned the same result.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

pub mod cli;
pub mod data;
pub mod engine;
pub mod fingerprint;
pub mod ladybug;
pub mod measure;
pub mod random;
pub mod report;
pub mod rhizo;
pub mod runner;
pub mod scale;
pub mod workloads;
