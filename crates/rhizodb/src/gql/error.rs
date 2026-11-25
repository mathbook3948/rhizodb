use std::error::Error;
use std::fmt;

/// Why the query text was rejected by the parser, carried by
/// [`ErrorKind::Syntax`](crate::ErrorKind::Syntax).
///
/// ```
/// use rhizodb::Database;
/// use rhizodb::ErrorKind;
/// use rhizodb::Parameters;
/// use rhizodb::SyntaxErrorKind;
///
/// let db = Database::in_memory()?;
/// let error = db.execute("MATCH (n", &Parameters::new()).unwrap_err();
/// assert_eq!(error.kind(), ErrorKind::Syntax(SyntaxErrorKind::UnexpectedEnd));
/// # Ok::<(), rhizodb::Error>(())
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SyntaxErrorKind {
    /// A token appeared where the grammar does not allow it.
    UnexpectedToken,
    /// The query ended before a construct was complete.
    UnexpectedEnd,
    /// A character that starts no token.
    UnexpectedCharacter,
    /// A malformed number literal.
    InvalidNumberLiteral,
    /// An integer literal outside the 64-bit signed range.
    IntegerOverflow,
    /// A float literal too large for a 64-bit float.
    FloatOverflow,
    /// A character outside ASCII that starts no token, such as a dash look-alike.
    InvalidUnicodeCharacter,
    /// An unknown escape sequence in a string literal.
    InvalidEscape,
    /// A malformed `\u` escape sequence.
    InvalidUnicodeEscape,
    /// A string literal without its closing quote.
    UnterminatedString,
    /// A backtick-quoted name without its closing backtick.
    UnterminatedQuotedName,
    /// A block comment without its closing `*/`.
    UnterminatedComment,
    /// Length bounds on a relationship without the `*` before them, or a negative bound.
    InvalidRelationshipPattern,
    /// `UNION` and `UNION ALL`, or two different set operators, in one query.
    MixedUnion,
    /// Syntax that is recognized but not supported.
    UnsupportedFeature,
    /// Expressions or patterns nested past the parser's limit.
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
            Self::FloatOverflow => "float literal does not fit in 64 bits",
            Self::InvalidUnicodeCharacter => "invalid unicode character",
            Self::InvalidEscape => "invalid escape sequence",
            Self::InvalidUnicodeEscape => "invalid unicode escape sequence",
            Self::UnterminatedString => "unterminated string literal",
            Self::UnterminatedQuotedName => "unterminated quoted name",
            Self::UnterminatedComment => "unterminated comment",
            Self::InvalidRelationshipPattern => "invalid relationship length",
            Self::MixedUnion => "one query cannot mix set operators or UNION and UNION ALL",
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
