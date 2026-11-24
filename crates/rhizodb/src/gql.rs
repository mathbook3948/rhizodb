pub(crate) mod ast;
pub(crate) mod error;
pub(crate) mod lexer;
pub(crate) mod parser;

#[cfg(test)]
mod testing;

#[cfg(test)]
#[path = "gql/corpus.test.rs"]
mod corpus_tests;

/// Whether `input` parses as a query, for the parser fuzz target.
#[cfg(feature = "fuzzing")]
pub fn parse_for_fuzzing(input: &str) -> bool {
    parser::parse(input).is_ok()
}
