use std::cell::RefCell;
use std::collections::VecDeque;
use std::fmt::Display;

use regex_automata::meta::Regex;
use regex_syntax::Parser;
use regex_syntax::hir::Hir;
use regex_syntax::hir::Look;

use crate::error::Error;
use crate::error::ErrorKind;

const COMPILED_SIZE_LIMIT: usize = 10 << 20;
const CACHED_PATTERNS: usize = 8;

/// The patterns one query compiled, so a pattern that many rows read compiles once.
#[derive(Debug, Default)]
pub(crate) struct Patterns {
    compiled: RefCell<VecDeque<(String, Regex)>>,
}

impl Patterns {
    /// Whether the regular expression `pattern` matches all of `text`.
    pub(crate) fn matches(&self, text: &str, pattern: &str) -> Result<bool, Error> {
        if let Some((_, regex)) = self
            .compiled
            .borrow()
            .iter()
            .find(|(cached, _)| cached == pattern)
        {
            return Ok(regex.is_match(text));
        }
        let regex = compile(pattern)?;
        let matched = regex.is_match(text);
        let mut compiled = self.compiled.borrow_mut();
        if compiled.len() >= CACHED_PATTERNS {
            compiled.pop_front();
        }
        compiled.push_back((pattern.to_owned(), regex));
        Ok(matched)
    }
}

fn compile(pattern: &str) -> Result<Regex, Error> {
    let hir = Parser::new()
        .parse(pattern)
        .map_err(|error| invalid(pattern, syntax_reason(&error)))?;
    let whole = Hir::concat(vec![Hir::look(Look::Start), hir, Hir::look(Look::End)]);
    Regex::builder()
        .configure(Regex::config().nfa_size_limit(Some(COMPILED_SIZE_LIMIT)))
        .build_from_hir(&whole)
        .map_err(|error| match error.size_limit() {
            Some(limit) => invalid(
                pattern,
                format!("it compiles past the limit of {limit} bytes"),
            ),
            None => invalid(pattern, error),
        })
}

fn syntax_reason(error: &regex_syntax::Error) -> String {
    match error {
        regex_syntax::Error::Parse(error) => error.kind().to_string(),
        regex_syntax::Error::Translate(error) => error.kind().to_string(),
        other => other.to_string(),
    }
}

fn invalid(pattern: &str, reason: impl Display) -> Error {
    Error::new(
        ErrorKind::InvalidRegex,
        format!("invalid regular expression {pattern:?}: {reason}"),
    )
}

#[cfg(test)]
#[path = "regex.test.rs"]
mod tests;
