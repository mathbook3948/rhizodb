//! Runs the vendored openCypher TCK against a pluggable engine.
//!
//! Feature files compile into typed [`scenario::Scenario`]s, an [`engine::Engine`] answers their
//! queries, and a checked-in ratchet in `passing.txt` records which scenarios must keep passing.

pub mod cli;
pub mod engine;
pub mod error;
#[cfg(test)]
mod fake;
pub mod feature;
pub mod kuzu;
pub mod literal;
pub mod matching;
pub mod parser;
pub mod ratchet;
pub mod rhizo;
pub mod run;
pub mod scenario;
pub mod side_effects;
pub mod signature;
pub mod steps;
pub mod suite;
pub mod value;
