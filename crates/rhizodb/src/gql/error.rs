use std::error::Error;
use std::fmt;

/// Why the query text was rejected.
///
/// The variants are fine-grained enough for a later layer to map them onto
/// the openCypher TCK detail codes (`UnexpectedSyntax`, `InvalidNumberLiteral`,
/// `InvalidUnicodeLiteral`, `IntegerOverflow`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum SyntaxErrorKind {
    UnexpectedToken,
    UnexpectedEnd,
    UnexpectedCharacter,
    InvalidNumberLiteral,
    IntegerOverflow,
    InvalidEscape,
    InvalidUnicodeEscape,
    UnterminatedString,
    UnterminatedQuotedName,
    UnterminatedComment,
    MixedUnion,
    UnsupportedFeature,
    NestingTooDeep,
}

impl SyntaxErrorKind {
    fn description(self) -> &'static str {
        match self {
            Self::UnexpectedToken => "unexpected token",
            Self::UnexpectedEnd => "unexpected end of query",
            Self::UnexpectedCharacter => "unexpected character",
            Self::InvalidNumberLiteral => "invalid number literal",
            Self::IntegerOverflow => "integer literal does not fit in 64 bits",
            Self::InvalidEscape => "invalid escape sequence",
            Self::InvalidUnicodeEscape => "invalid unicode escape sequence",
            Self::UnterminatedString => "unterminated string literal",
            Self::UnterminatedQuotedName => "unterminated quoted name",
            Self::UnterminatedComment => "unterminated comment",
            Self::MixedUnion => "UNION and UNION ALL cannot be mixed",
            Self::UnsupportedFeature => "unsupported feature",
            Self::NestingTooDeep => "query is nested too deeply",
        }
    }
}

/// A rejected query: what went wrong and the byte offset where it was noticed.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct SyntaxError {
    pub offset: usize,
    pub kind: SyntaxErrorKind,
}

impl SyntaxError {
    pub(crate) fn new(offset: usize, kind: SyntaxErrorKind) -> Self {
        Self { offset, kind }
    }
}

impl fmt::Display for SyntaxError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} at byte {}",
            self.kind.description(),
            self.offset
        )
    }
}

impl Error for SyntaxError {}
