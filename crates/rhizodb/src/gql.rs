#![doc(hidden)]
#![allow(
    dead_code,
    reason = "the executor that consumes the syntax tree does not exist yet"
)]

pub(crate) mod ast;
pub(crate) mod error;
pub(crate) mod lexer;
pub(crate) mod parser;

#[cfg(test)]
mod testing;

#[cfg(test)]
#[path = "gql/corpus.test.rs"]
mod corpus_tests;

pub fn parse_for_fuzzing(input: &str) -> bool {
    parser::parse(input).is_ok()
}
