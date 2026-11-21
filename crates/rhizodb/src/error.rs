use std::error::Error as StdError;
use std::fmt;

use crate::gql::error::SyntaxError;
use crate::gql::error::SyntaxErrorKind;
use crate::graph::GraphError;
use crate::storage::StorageError;

/// The condition that made a query fail. Callers branch on this, never on the message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorKind {
    /// The parser rejected the query text.
    Syntax(SyntaxErrorKind),
    /// A variable is used where it is not in scope.
    UndefinedVariable,
    /// A variable is introduced again where it is already bound.
    VariableAlreadyBound,
    /// A variable is used as a node, relationship, or path when it is bound to something else.
    VariableTypeConflict,
    /// Two projected columns share a name.
    ColumnNameConflict,
    /// A `WITH` item that is not a plain variable has no alias.
    NoExpressionAlias,
    /// `RETURN *` or `WITH *` with no variable in scope.
    NoVariablesInScope,
    /// Clauses in an order the language does not allow, such as `RETURN` before another clause or
    /// a query that ends with a reading clause.
    InvalidClauseComposition,
    /// The parts of a `UNION` project different columns.
    DifferentColumnsInUnion,
    /// An aggregate appears where aggregation is not allowed.
    InvalidAggregation,
    /// An aggregate appears inside another aggregate.
    NestedAggregation,
    /// An aggregating projection uses a variable that is neither a grouping key nor aggregated.
    AmbiguousAggregationExpression,
    /// `SKIP` or `LIMIT` refers to a variable.
    NonConstantExpression,
    /// `SKIP` or `LIMIT` is negative.
    NegativeIntegerArgument,
    /// A relationship to create names no type or more than one.
    NoSingleRelationshipType,
    /// A relationship to create has no direction.
    RequiresDirectedRelationship,
    /// A variable-length relationship in a pattern that creates.
    CreatingVarLength,
    /// A parameter is used where only a literal map is allowed.
    InvalidParameterUse,
    /// `DELETE` names something that is not a node, relationship, or path.
    InvalidDelete,
    /// One relationship variable appears twice in one pattern.
    RelationshipUniquenessViolation,
    /// A function name that does not exist.
    UnknownFunction,
    /// A function called with the wrong number of arguments.
    InvalidNumberOfArguments,
    /// A query refers to a parameter that was not supplied.
    MissingParameter,
    /// The query text itself applies an operator or clause to a value of a type it does not
    /// accept, so it fails before reading any data: `RETURN 1 AND 'a'`, `SKIP 1.5`.
    InvalidArgumentType,
    /// An operator or clause got a value of a type it does not accept from the data it read.
    TypeMismatch,
    /// A function got a value of the right type that it cannot convert or use.
    InvalidArgumentValue,
    /// A property was read from a value that is not a node, relationship, or map.
    PropertyAccessOnNonEntity,
    /// A list was indexed with something that is not an integer.
    ListElementAccessByNonInteger,
    /// A map was indexed with something that is not a string.
    MapElementAccessByNonString,
    /// A value cannot be stored as a property: a map, a graph element, a list holding null, or a
    /// list nested past the stored limit.
    InvalidPropertyType,
    /// Integer arithmetic left the 64-bit signed range.
    IntegerOverflow,
    /// Integer division or modulo by zero.
    DivisionByZero,
    /// A numeric argument outside the range the function accepts.
    NumberOutOfRange,
    /// A node to delete still has relationships and the delete is not `DETACH`.
    DeleteConnectedNode,
    /// A node or relationship deleted earlier in the query was read.
    DeletedEntityAccess,
    /// A feature this version does not support.
    Unsupported,
    /// The storage engine failed.
    Storage,
    /// Stored data does not decode or breaks an invariant.
    CorruptData,
    /// The database was written in a format version this build cannot read.
    UnsupportedFormatVersion,
    /// No node, relationship, or token identifiers are left.
    IdSpaceExhausted,
}

/// A failed query or database operation.
#[derive(Debug)]
pub struct Error {
    kind: ErrorKind,
    message: String,
    source: Option<Box<dyn StdError + Send + Sync>>,
}

impl Error {
    pub(crate) fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            source: None,
        }
    }

    /// The condition that caused the failure.
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// The same failure, classified as `kind` instead.
    pub(crate) fn with_kind(self, kind: ErrorKind) -> Self {
        Self { kind, ..self }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl StdError for Error {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn StdError + 'static))
    }
}

impl From<SyntaxError> for Error {
    fn from(error: SyntaxError) -> Self {
        Self::new(ErrorKind::Syntax(error.kind), error.to_string())
    }
}

impl From<StorageError> for Error {
    fn from(error: StorageError) -> Self {
        Self {
            kind: ErrorKind::Storage,
            message: error.to_string(),
            source: Some(Box::new(error)),
        }
    }
}

impl From<GraphError> for Error {
    fn from(error: GraphError) -> Self {
        let kind = match &error {
            GraphError::Storage(_) => ErrorKind::Storage,
            GraphError::Decode(_) | GraphError::Corrupt(_) => ErrorKind::CorruptData,
            GraphError::UnsupportedFormatVersion(_) => ErrorKind::UnsupportedFormatVersion,
            GraphError::IdSpaceExhausted => ErrorKind::IdSpaceExhausted,
            GraphError::NodeNotFound(_) | GraphError::RelationshipNotFound(_) => {
                ErrorKind::DeletedEntityAccess
            }
            GraphError::NodeHasRelationships(_) => ErrorKind::DeleteConnectedNode,
        };
        Self {
            kind,
            message: error.to_string(),
            source: Some(Box::new(error)),
        }
    }
}
