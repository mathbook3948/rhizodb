use super::error::SyntaxError;
use super::error::SyntaxErrorKind;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Punct {
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Comma,
    Colon,
    Semicolon,
    Dot,
    DotDot,
    Pipe,
    Ampersand,
    Bang,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Caret,
    Eq,
    Neq,
    Lt,
    Le,
    Gt,
    Ge,
    PlusEq,
    RegexMatch,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum TokenKind {
    /// A bare or backtick-quoted name. Only bare words can be keywords.
    Word {
        text: String,
        quoted: bool,
    },
    /// The magnitude of an integer literal. The sign is a separate token.
    Integer(u64),
    Float(f64),
    /// A malformed number literal, such as `12abc` or `0x`, kept as one token so the parser
    /// reports it as an invalid number where it expects a value and as unexpected elsewhere.
    InvalidNumber,
    String(String),
    Parameter(String),
    Punct(Punct),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Token {
    pub kind: TokenKind,
    pub start: usize,
    pub end: usize,
}

pub(crate) fn lex(input: &str) -> Result<Vec<Token>, SyntaxError> {
    let mut lexer = Lexer { input, pos: 0 };
    let mut tokens = Vec::new();
    loop {
        lexer.skip_trivia()?;
        let start = lexer.pos;
        let Some(first) = lexer.peek() else {
            return Ok(tokens);
        };
        let kind = lexer.token(first)?;
        tokens.push(Token {
            kind,
            start,
            end: lexer.pos,
        });
    }
}

struct Lexer<'a> {
    input: &'a str,
    pos: usize,
}

fn is_word_start(candidate: char) -> bool {
    candidate.is_alphabetic() || candidate == '_'
}

fn is_word_continue(candidate: char) -> bool {
    candidate.is_alphanumeric() || candidate == '_'
}

impl Lexer<'_> {
    fn rest(&self) -> &str {
        &self.input[self.pos..]
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn peek_second(&self) -> Option<char> {
        self.rest().chars().nth(1)
    }

    fn bump(&mut self) -> Option<char> {
        let next = self.peek()?;
        self.pos += next.len_utf8();
        Some(next)
    }

    fn eat(&mut self, expected: char) -> bool {
        if self.peek() == Some(expected) {
            self.pos += expected.len_utf8();
            true
        } else {
            false
        }
    }

    fn error(offset: usize, kind: SyntaxErrorKind) -> SyntaxError {
        SyntaxError::new(offset, kind)
    }

    fn skip_trivia(&mut self) -> Result<(), SyntaxError> {
        loop {
            match (self.peek(), self.peek_second()) {
                (Some(whitespace), _) if whitespace.is_whitespace() => {
                    self.bump();
                }
                (Some('/'), Some('/')) => {
                    while let Some(next) = self.bump() {
                        if next == '\n' {
                            break;
                        }
                    }
                }
                (Some('/'), Some('*')) => {
                    let start = self.pos;
                    self.pos += 2;
                    match self.rest().find("*/") {
                        Some(offset) => self.pos += offset + 2,
                        None => {
                            return Err(Self::error(start, SyntaxErrorKind::UnterminatedComment));
                        }
                    }
                }
                _ => return Ok(()),
            }
        }
    }

    fn token(&mut self, first: char) -> Result<TokenKind, SyntaxError> {
        let start = self.pos;
        if first.is_ascii_digit()
            || (first == '.' && self.peek_second().is_some_and(|c| c.is_ascii_digit()))
        {
            return self.number(start);
        }
        if is_word_start(first) {
            let text = self.take_while(is_word_continue);
            return Ok(TokenKind::Word {
                text: text.to_owned(),
                quoted: false,
            });
        }
        match first {
            '\'' | '"' => self.string(first, start),
            '`' => self.quoted_name(start),
            '$' => self.parameter(start),
            _ => self.punctuation(first, start),
        }
    }

    fn take_while(&mut self, accept: impl Fn(char) -> bool) -> &str {
        let start = self.pos;
        while self.peek().is_some_and(&accept) {
            self.bump();
        }
        &self.input[start..self.pos]
    }

    fn punctuation(&mut self, first: char, start: usize) -> Result<TokenKind, SyntaxError> {
        self.bump();
        let punct = match first {
            '(' => Punct::LParen,
            ')' => Punct::RParen,
            '[' => Punct::LBracket,
            ']' => Punct::RBracket,
            '{' => Punct::LBrace,
            '}' => Punct::RBrace,
            ',' => Punct::Comma,
            ':' => Punct::Colon,
            ';' => Punct::Semicolon,
            '|' => Punct::Pipe,
            '&' => Punct::Ampersand,
            '!' => Punct::Bang,
            '*' => Punct::Star,
            '/' => Punct::Slash,
            '%' => Punct::Percent,
            '^' => Punct::Caret,
            '-' => Punct::Minus,
            '.' if self.eat('.') => Punct::DotDot,
            '.' => Punct::Dot,
            '+' if self.eat('=') => Punct::PlusEq,
            '+' => Punct::Plus,
            '=' if self.eat('~') => Punct::RegexMatch,
            '=' => Punct::Eq,
            '<' if self.eat('>') => Punct::Neq,
            '<' if self.eat('=') => Punct::Le,
            '<' => Punct::Lt,
            '>' if self.eat('=') => Punct::Ge,
            '>' => Punct::Gt,
            _ if !first.is_ascii() => {
                return Err(Self::error(start, SyntaxErrorKind::InvalidUnicodeCharacter));
            }
            _ => {
                return Err(Self::error(start, SyntaxErrorKind::UnexpectedCharacter));
            }
        };
        Ok(TokenKind::Punct(punct))
    }

    fn parameter(&mut self, start: usize) -> Result<TokenKind, SyntaxError> {
        self.bump();
        let name = self.take_while(is_word_continue);
        if name.is_empty() {
            return Err(Self::error(start, SyntaxErrorKind::UnexpectedCharacter));
        }
        Ok(TokenKind::Parameter(name.to_owned()))
    }

    fn quoted_name(&mut self, start: usize) -> Result<TokenKind, SyntaxError> {
        self.bump();
        let mut text = String::new();
        loop {
            match self.bump() {
                None => {
                    return Err(Self::error(start, SyntaxErrorKind::UnterminatedQuotedName));
                }
                Some('`') if self.eat('`') => text.push('`'),
                Some('`') => return Ok(TokenKind::Word { text, quoted: true }),
                Some(other) => text.push(other),
            }
        }
    }

    fn string(&mut self, quote: char, start: usize) -> Result<TokenKind, SyntaxError> {
        self.bump();
        let mut text = String::new();
        loop {
            let at = self.pos;
            match self.bump() {
                None => return Err(Self::error(start, SyntaxErrorKind::UnterminatedString)),
                Some(close) if close == quote => return Ok(TokenKind::String(text)),
                Some('\\') => text.push(self.escape(at)?),
                Some(other) => text.push(other),
            }
        }
    }

    fn escape(&mut self, backslash: usize) -> Result<char, SyntaxError> {
        let Some(code) = self.bump() else {
            return Err(Self::error(backslash, SyntaxErrorKind::InvalidEscape));
        };
        match code {
            '\\' | '\'' | '"' => Ok(code),
            'b' => Ok('\u{8}'),
            'f' => Ok('\u{c}'),
            'n' => Ok('\n'),
            'r' => Ok('\r'),
            't' => Ok('\t'),
            'u' => self.unicode_escape(backslash, 4),
            'U' => self.unicode_escape(backslash, 8),
            _ => Err(Self::error(backslash, SyntaxErrorKind::InvalidEscape)),
        }
    }

    fn hex_digits(&mut self, count: usize) -> Option<u32> {
        let digits = self.rest().get(..count)?;
        if !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        let value = u32::from_str_radix(digits, 16).ok()?;
        self.pos += count;
        Some(value)
    }

    fn unicode_escape(&mut self, backslash: usize, width: usize) -> Result<char, SyntaxError> {
        let invalid = || Self::error(backslash, SyntaxErrorKind::InvalidUnicodeEscape);
        let unit = self.hex_digits(width).ok_or_else(invalid)?;
        if (0xD800..0xDC00).contains(&unit) && width == 4 {
            if !self.rest().starts_with("\\u") {
                return Err(invalid());
            }
            self.pos += 2;
            let low = self.hex_digits(4).ok_or_else(invalid)?;
            if !(0xDC00..0xE000).contains(&low) {
                return Err(invalid());
            }
            let combined = 0x10000 + ((unit - 0xD800) << 10) + (low - 0xDC00);
            return char::from_u32(combined).ok_or_else(invalid);
        }
        char::from_u32(unit).ok_or_else(invalid)
    }

    fn number(&mut self, start: usize) -> Result<TokenKind, SyntaxError> {
        match self.number_literal(start)? {
            Some(kind) => Ok(kind),
            None => {
                self.take_while(is_word_continue);
                Ok(TokenKind::InvalidNumber)
            }
        }
    }

    fn number_literal(&mut self, start: usize) -> Result<Option<TokenKind>, SyntaxError> {
        let radix = match self.rest().as_bytes() {
            [b'0', b'x' | b'X', ..] => Some(16),
            [b'0', b'o' | b'O', ..] => Some(8),
            _ => None,
        };
        if let Some(radix) = radix {
            self.pos += 2;
            let digits = self.take_while(|c| c.is_digit(radix)).to_owned();
            if digits.is_empty() || self.peek().is_some_and(is_word_continue) {
                return Ok(None);
            }
            return Self::integer(&digits, radix, start).map(Some);
        }
        let mut is_float = false;
        self.take_while(|c| c.is_ascii_digit());
        if self.peek() == Some('.') && self.peek_second().is_some_and(|c| c.is_ascii_digit()) {
            is_float = true;
            self.bump();
            self.take_while(|c| c.is_ascii_digit());
        }
        if matches!(self.peek(), Some('e' | 'E')) {
            is_float = true;
            self.bump();
            if matches!(self.peek(), Some('+' | '-')) {
                self.bump();
            }
            if self.take_while(|c| c.is_ascii_digit()).is_empty() {
                return Ok(None);
            }
        }
        if self.peek().is_some_and(is_word_continue) {
            return Ok(None);
        }
        let text = &self.input[start..self.pos];
        if is_float {
            return match text.parse::<f64>() {
                Ok(value) if value.is_finite() => Ok(Some(TokenKind::Float(value))),
                Ok(_) => Err(Self::error(start, SyntaxErrorKind::FloatOverflow)),
                Err(_) => Ok(None),
            };
        }
        Self::integer(text, 10, start).map(Some)
    }

    fn integer(digits: &str, radix: u32, start: usize) -> Result<TokenKind, SyntaxError> {
        u64::from_str_radix(digits, radix)
            .map(TokenKind::Integer)
            .map_err(|_| Self::error(start, SyntaxErrorKind::IntegerOverflow))
    }
}

#[cfg(test)]
#[path = "lexer.test.rs"]
mod tests;
