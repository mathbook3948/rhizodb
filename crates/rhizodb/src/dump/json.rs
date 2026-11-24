use std::fmt::Write as _;

pub(crate) const MAX_NESTING: usize = 80;

/// A parsed JSON value. A number without a fraction or an exponent is an integer, and one with
/// either is a float, so the two stay apart.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Json {
    Null,
    Boolean(bool),
    Integer(i64),
    Float(f64),
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

/// Parses one JSON text that fills `text` but for surrounding whitespace.
pub(crate) fn parse(text: &str) -> Result<Json, String> {
    let mut parser = Parser {
        bytes: text.as_bytes(),
        at: 0,
    };
    parser.whitespace();
    let value = parser.value(0)?;
    parser.whitespace();
    if parser.at != parser.bytes.len() {
        return Err(parser.error("text follows the value"));
    }
    Ok(value)
}

struct Parser<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Parser<'_> {
    fn error(&self, what: &str) -> String {
        format!("{what} at byte {}", self.at)
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    fn whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    fn expect(&mut self, byte: u8) -> Result<(), String> {
        if self.peek() == Some(byte) {
            self.at += 1;
            Ok(())
        } else {
            Err(self.error(&format!("expected `{}`", char::from(byte))))
        }
    }

    fn literal(&mut self, word: &str, value: Json) -> Result<Json, String> {
        if self.bytes[self.at..].starts_with(word.as_bytes()) {
            self.at += word.len();
            Ok(value)
        } else {
            Err(self.error("unknown literal"))
        }
    }

    fn value(&mut self, depth: usize) -> Result<Json, String> {
        match self.peek() {
            Some(b'{') => self.object(depth),
            Some(b'[') => self.array(depth),
            Some(b'"') => self.string().map(Json::String),
            Some(b't') => self.literal("true", Json::Boolean(true)),
            Some(b'f') => self.literal("false", Json::Boolean(false)),
            Some(b'n') => self.literal("null", Json::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) => Err(self.error("unexpected character")),
            None => Err(self.error("the text ends before a value")),
        }
    }

    fn nested(&self, depth: usize) -> Result<usize, String> {
        if depth >= MAX_NESTING {
            Err(self.error("arrays and objects nest too deep"))
        } else {
            Ok(depth + 1)
        }
    }

    fn object(&mut self, depth: usize) -> Result<Json, String> {
        let depth = self.nested(depth)?;
        self.expect(b'{')?;
        self.whitespace();
        let mut entries = Vec::new();
        if self.peek() == Some(b'}') {
            self.at += 1;
            return Ok(Json::Object(entries));
        }
        loop {
            self.whitespace();
            if self.peek() != Some(b'"') {
                return Err(self.error("expected a string key"));
            }
            let key = self.string()?;
            self.whitespace();
            self.expect(b':')?;
            self.whitespace();
            let value = self.value(depth)?;
            entries.push((key, value));
            self.whitespace();
            match self.peek() {
                Some(b',') => self.at += 1,
                Some(b'}') => {
                    self.at += 1;
                    return Ok(Json::Object(entries));
                }
                _ => return Err(self.error("expected `,` or `}`")),
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<Json, String> {
        let depth = self.nested(depth)?;
        self.expect(b'[')?;
        self.whitespace();
        let mut items = Vec::new();
        if self.peek() == Some(b']') {
            self.at += 1;
            return Ok(Json::Array(items));
        }
        loop {
            self.whitespace();
            items.push(self.value(depth)?);
            self.whitespace();
            match self.peek() {
                Some(b',') => self.at += 1,
                Some(b']') => {
                    self.at += 1;
                    return Ok(Json::Array(items));
                }
                _ => return Err(self.error("expected `,` or `]`")),
            }
        }
    }

    fn digits(&mut self) -> usize {
        let start = self.at;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.at += 1;
        }
        self.at - start
    }

    fn number(&mut self) -> Result<Json, String> {
        let start = self.at;
        if self.peek() == Some(b'-') {
            self.at += 1;
        }
        let first = self.peek();
        let whole = self.digits();
        if whole == 0 || (first == Some(b'0') && whole > 1) {
            return Err(self.error("malformed number"));
        }
        let mut float = false;
        if self.peek() == Some(b'.') {
            self.at += 1;
            float = true;
            if self.digits() == 0 {
                return Err(self.error("malformed number"));
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.at += 1;
            float = true;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.at += 1;
            }
            if self.digits() == 0 {
                return Err(self.error("malformed number"));
            }
        }
        let text = std::str::from_utf8(&self.bytes[start..self.at])
            .map_err(|_| self.error("malformed number"))?;
        if float {
            match text.parse::<f64>() {
                Ok(value) if value.is_finite() => Ok(Json::Float(value)),
                _ => Err(self.error("a float out of range")),
            }
        } else {
            text.parse::<i64>()
                .map(Json::Integer)
                .map_err(|_| self.error("an integer out of the 64-bit range"))
        }
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let digits = self
            .bytes
            .get(self.at..self.at + 4)
            .ok_or_else(|| self.error("a short `\\u` escape"))?;
        let mut value = 0u32;
        for digit in digits {
            let nibble = char::from(*digit)
                .to_digit(16)
                .ok_or_else(|| self.error("a malformed `\\u` escape"))?;
            value = value * 16 + nibble;
        }
        self.at += 4;
        Ok(value)
    }

    fn string(&mut self) -> Result<String, String> {
        self.expect(b'"')?;
        let mut out = Vec::new();
        loop {
            let byte = self
                .peek()
                .ok_or_else(|| self.error("the text ends inside a string"))?;
            self.at += 1;
            match byte {
                b'"' => {
                    return String::from_utf8(out).map_err(|_| self.error("invalid UTF-8"));
                }
                b'\\' => {
                    let escape = self
                        .peek()
                        .ok_or_else(|| self.error("the text ends inside an escape"))?;
                    self.at += 1;
                    let decoded = match escape {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => self.unicode_escape()?,
                        _ => return Err(self.error("an unknown escape")),
                    };
                    let mut buffer = [0u8; 4];
                    out.extend_from_slice(decoded.encode_utf8(&mut buffer).as_bytes());
                }
                0x00..=0x1f => return Err(self.error("a control character inside a string")),
                _ => out.push(byte),
            }
        }
    }

    fn unicode_escape(&mut self) -> Result<char, String> {
        let first = self.hex4()?;
        let code = if (0xd800..0xdc00).contains(&first) {
            if !self.bytes[self.at..].starts_with(b"\\u") {
                return Err(self.error("a lone surrogate"));
            }
            self.at += 2;
            let second = self.hex4()?;
            if !(0xdc00..0xe000).contains(&second) {
                return Err(self.error("a lone surrogate"));
            }
            0x10000 + ((first - 0xd800) << 10) + (second - 0xdc00)
        } else {
            first
        };
        char::from_u32(code).ok_or_else(|| self.error("a lone surrogate"))
    }
}

/// Appends `text` as a JSON string, escaping only `"`, `\`, and control characters.
pub(crate) fn write_string(out: &mut String, text: &str) {
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{0}'..='\u{1f}' => {
                let _ = write!(out, "\\u{:04x}", u32::from(character));
            }
            other => out.push(other),
        }
    }
    out.push('"');
}

/// Appends a finite float as the shortest JSON number that reads back as it, always with a
/// fraction or an exponent so it never reads back as an integer.
pub(crate) fn write_finite_float(out: &mut String, value: f64) {
    let start = out.len();
    let _ = write!(out, "{value:?}");
    if !out[start..].contains(['.', 'e', 'E']) {
        out.push_str(".0");
    }
}
