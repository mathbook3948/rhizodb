//! Runs the same queries on RhizoDB and LadybugDB and compares the results.
//!
//! The corpus joins a hand-written file, queries generated from a seed over a generated graph,
//! and the graph-free scenarios of the vendored TCK. A checked-in list in `differences.txt`
//! names every query on which the engines do not agree, with the reason for each difference.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

pub mod cli;
pub mod compare;
pub mod corpus;
pub mod expectations;
pub mod graph;
pub mod ladybug;
pub mod random;
pub mod run;
