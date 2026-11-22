use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;

use crate::value::Direction;
use crate::value::Node;
use crate::value::Path;
use crate::value::PathStep;
use crate::value::Properties;
use crate::value::Relationship;
use crate::value::Value;

/// A cell that is not valid TCK literal syntax.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiteralError {
    /// The text that was being parsed.
    pub text: String,
    /// Byte offset of the first problem.
    pub position: usize,
    /// What the parser expected there.
    pub expected: &'static str,
}

impl fmt::Display for LiteralError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "expected {} at byte {} of `{}`",
            self.expected, self.position, self.text
        )
    }
}

impl Error for LiteralError {}

/// Parses one value written in the TCK literal syntax of result and parameter tables.
pub fn parse_value(text: &str) -> Result<Value, LiteralError> {
    let mut cursor = Cursor { text, position: 0 };
    let value = cursor.value()?;
    cursor.skip_whitespace();
    if cursor.position < text.len() {
        return Err(cursor.error("end of input"));
    }
    Ok(value)
}

struct Cursor<'a> {
    text: &'a str,
    position: usize,
}

impl Cursor<'_> {
    fn error(&self, expected: &'static str) -> LiteralError {
        LiteralError {
            text: self.text.to_string(),
            position: self.position,
            expected,
        }
    }

    fn rest(&self) -> &str {
        &self.text[self.position..]
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn skip_whitespace(&mut self) {
        while let Some(c) = self.peek().filter(|c| c.is_whitespace()) {
            self.position += c.len_utf8();
        }
    }

    fn eat(&mut self, token: &str) -> bool {
        let found = self.rest().starts_with(token);
        if found {
            self.position += token.len();
        }
        found
    }

    fn expect(&mut self, token: &'static str) -> Result<(), LiteralError> {
        self.skip_whitespace();
        if self.eat(token) {
            Ok(())
        } else {
            Err(self.error(token))
        }
    }

    fn take_while(&mut self, accept: impl Fn(char) -> bool) -> &str {
        let start = self.position;
        while let Some(c) = self.peek().filter(|c| accept(*c)) {
            self.position += c.len_utf8();
        }
        &self.text[start..self.position]
    }

    fn value(&mut self) -> Result<Value, LiteralError> {
        self.skip_whitespace();
        match self.peek() {
            Some('(') => Ok(Value::Node(self.node()?)),
            Some('[') => {
                let mut ahead = self.rest()[1..].trim_start().chars();
                if ahead.next() == Some(':') {
                    Ok(Value::Relationship(self.relationship()?))
                } else {
                    self.list()
                }
            }
            Some('<') => self.path(),
            Some('{') => Ok(Value::Map(self.map()?)),
            Some('\'') => Ok(Value::String(self.string()?)),
            Some(c) if c.is_ascii_digit() || matches!(c, '-' | '.') => self.number(),
            _ => self.keyword(),
        }
    }

    fn keyword(&mut self) -> Result<Value, LiteralError> {
        const KEYWORDS: [(&str, Value); 4] = [
            ("true", Value::Boolean(true)),
            ("false", Value::Boolean(false)),
            ("null", Value::Null),
            ("NaN", Value::Float(f64::NAN)),
        ];
        for (word, value) in KEYWORDS {
            if self.eat(word) {
                return Ok(value);
            }
        }
        for word in ["Infinity", "Inf"] {
            if self.eat(word) {
                return Ok(Value::Float(f64::INFINITY));
            }
        }
        Err(self.error("a value"))
    }

    fn number(&mut self) -> Result<Value, LiteralError> {
        let start = self.position;
        let negative = self.eat("-");
        if self.eat("Infinity") || self.eat("Inf") {
            let infinity = f64::INFINITY;
            return Ok(Value::Float(if negative { -infinity } else { infinity }));
        }
        let integer_digits = self.take_while(|c| c.is_ascii_digit()).len();
        let mut is_float = false;
        if self.eat(".") {
            is_float = true;
            if self.take_while(|c| c.is_ascii_digit()).is_empty() {
                return Err(self.error("digits after the decimal point"));
            }
        } else if integer_digits == 0 {
            return Err(self.error("digits"));
        }
        if self.rest().starts_with(['e', 'E']) {
            is_float = true;
            self.position += 1;
            if !self.eat("-") {
                self.eat("+");
            }
            if self.take_while(|c| c.is_ascii_digit()).is_empty() {
                return Err(self.error("exponent digits"));
            }
        }
        let literal = &self.text[start..self.position];
        let failed = LiteralError {
            text: self.text.to_string(),
            position: start,
            expected: if is_float {
                "a float"
            } else {
                "an integer in range"
            },
        };
        if is_float {
            literal.parse().map(Value::Float).map_err(|_| failed)
        } else {
            literal.parse().map(Value::Integer).map_err(|_| failed)
        }
    }

    fn string(&mut self) -> Result<String, LiteralError> {
        self.expect("'")?;
        let mut content = String::new();
        loop {
            match self.peek() {
                None => return Err(self.error("closing quote")),
                Some('\'') => {
                    self.position += 1;
                    return Ok(content);
                }
                Some('\\') => {
                    self.position += 1;
                    match self.peek() {
                        Some(escaped @ ('\'' | '\\')) => {
                            content.push(escaped);
                            self.position += 1;
                        }
                        _ => return Err(self.error("an escaped quote or backslash")),
                    }
                }
                Some(c) => {
                    content.push(c);
                    self.position += c.len_utf8();
                }
            }
        }
    }

    fn name(&mut self) -> Result<String, LiteralError> {
        let name = self.take_while(|c| c.is_ascii_alphanumeric() || c == '$' || c == '_');
        if name.is_empty() {
            Err(self.error("a name"))
        } else {
            Ok(name.to_string())
        }
    }

    fn list(&mut self) -> Result<Value, LiteralError> {
        self.expect("[")?;
        let mut elements = Vec::new();
        self.skip_whitespace();
        if self.eat("]") {
            return Ok(Value::List(elements));
        }
        loop {
            elements.push(self.value()?);
            self.skip_whitespace();
            if self.eat("]") {
                return Ok(Value::List(elements));
            }
            self.expect(",")?;
        }
    }

    fn map(&mut self) -> Result<Properties, LiteralError> {
        self.expect("{")?;
        let mut entries = Properties::new();
        self.skip_whitespace();
        if self.eat("}") {
            return Ok(entries);
        }
        loop {
            self.skip_whitespace();
            let key = self.name()?;
            self.expect(":")?;
            entries.insert(key, self.value()?);
            self.skip_whitespace();
            if self.eat("}") {
                return Ok(entries);
            }
            self.expect(",")?;
        }
    }

    fn optional_map(&mut self) -> Result<Properties, LiteralError> {
        self.skip_whitespace();
        if self.peek() == Some('{') {
            self.map()
        } else {
            Ok(Properties::new())
        }
    }

    fn node(&mut self) -> Result<Node, LiteralError> {
        self.expect("(")?;
        let mut labels = BTreeSet::new();
        self.skip_whitespace();
        while self.eat(":") {
            labels.insert(self.name()?);
            self.skip_whitespace();
        }
        let properties = self.optional_map()?;
        self.expect(")")?;
        Ok(Node { labels, properties })
    }

    fn relationship(&mut self) -> Result<Relationship, LiteralError> {
        self.expect("[")?;
        self.expect(":")?;
        let rel_type = self.name()?;
        let properties = self.optional_map()?;
        self.expect("]")?;
        Ok(Relationship {
            rel_type,
            properties,
        })
    }

    fn path(&mut self) -> Result<Value, LiteralError> {
        self.expect("<")?;
        let start = self.node()?;
        let mut steps = Vec::new();
        loop {
            let direction = if self.eat("-") {
                Direction::Forward
            } else if self.eat("<-") {
                Direction::Backward
            } else {
                break;
            };
            let relationship = self.relationship()?;
            match direction {
                Direction::Forward => self.expect("->")?,
                Direction::Backward => self.expect("-")?,
            }
            let node = self.node()?;
            steps.push(PathStep {
                relationship,
                direction,
                node,
            });
        }
        self.expect(">")?;
        Ok(Value::Path(Path { start, steps }))
    }
}

#[cfg(test)]
#[path = "literal.test.rs"]
mod tests;
