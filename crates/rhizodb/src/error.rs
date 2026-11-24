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
    /// `SKIP` or `LIMIT` refers to a variable, or an aggregate argument calls `rand()`.
    NonConstantExpression,
    /// `SKIP` or `LIMIT` is negative.
    NegativeIntegerArgument,
    /// A relationship to create names no type or more than one.
    NoSingleRelationshipType,
    /// A node to create, or a `SET` or `REMOVE` label item, has a label expression other than a
    /// label or a conjunction of labels, such as `A|B` or `!A`.
    NoLabelSet,
    /// A relationship to create has no direction.
    RequiresDirectedRelationship,
    /// A variable-length relationship in a pattern that creates.
    CreatingVarLength,
    /// A parameter is used where only a literal map is allowed.
    InvalidParameterUse,
    /// A `MERGE` pattern property evaluated to null, which can never match what the pattern creates.
    NullMergeProperty,
    /// `DELETE` names something that is not a node, relationship, or path.
    InvalidDelete,
    /// One relationship variable appears twice in one pattern.
    RelationshipUniquenessViolation,
    /// A function name that does not exist.
    UnknownFunction,
    /// A function called with the wrong number of arguments.
    InvalidNumberOfArguments,
    /// `CALL` names a procedure that is not registered.
    ProcedureNotFound,
    /// `YIELD` names an output the procedure does not declare.
    UnknownProcedureOutput,
    /// A `CALL` inside a larger query leaves out the arguments of a procedure that takes some.
    /// Only a query that is a single `CALL` reads them from the query parameters.
    InvalidArgumentPassingMode,
    /// A procedure body failed, or returned a row whose length differs from its outputs.
    ProcedureFailed,
    /// A query refers to a parameter that was not supplied.
    MissingParameter,
    /// The query text itself applies an operator or clause to a value of a type it does not
    /// accept, so it fails before reading any data: `RETURN 1 AND 'a'`, `SKIP 1.5`.
    InvalidArgumentType,
    /// An operator or clause got a value of a type it does not accept from the data it read.
    TypeMismatch,
    /// A function or aggregate got an argument it cannot use, found by evaluating the query
    /// rather than read off its text: an argument of the wrong type, such as `toUpper(x)` for a
    /// number `x`, or a percentile that differs between the rows of one group.
    InvalidArgumentValue,
    /// An argument of `range()`, which takes only integers, is a value of another type. Like
    /// [`ErrorKind::InvalidArgumentType`], it fails before any data is read when the query text
    /// decides the argument.
    NonIntegerArgument,
    /// A property was read from a value that is not a node, relationship, or map.
    PropertyAccessOnNonEntity,
    /// A list was indexed with something that is not an integer.
    ListElementAccessByNonInteger,
    /// A map was indexed with something that is not a string.
    MapElementAccessByNonString,
    /// A list or map nests deeper than a value in a query may: a parameter, a value a procedure
    /// returned, or a value the query builds.
    NestingTooDeep,
    /// A value cannot be stored as a property: a map, a graph element, a list holding null, or a
    /// list nested past the stored limit.
    InvalidPropertyType,
    /// Integer arithmetic left the 64-bit signed range.
    IntegerOverflow,
    /// Integer division or modulo by zero.
    DivisionByZero,
    /// The pattern of `=~` is not a valid regular expression, or compiles past the size limit.
    InvalidRegex,
    /// A numeric argument outside the range the function accepts.
    NumberOutOfRange,
    /// A temporal value that cannot be built or read: a malformed date or duration string, a
    /// component out of range, an unknown time zone, or a result past the supported years.
    InvalidTemporalValue,
    /// A node to delete still has relationships and the delete is not `DETACH`.
    DeleteConnectedNode,
    /// A node or relationship deleted earlier in the query was read.
    DeletedEntityAccess,
    /// A query that updates the graph ran in a read transaction.
    ReadOnlyTransaction,
    /// A write transaction was used after a query that failed in it rolled it back.
    TransactionAborted,
    /// A write was started on a thread that already holds the open write transaction, which
    /// would otherwise wait for itself forever.
    WriteTransactionOnThisThread,
    /// An index search names an index that does not exist, one of another kind, or one over the
    /// other kind of element.
    IndexNotFound,
    /// An index is created under a name that an index with another definition already holds.
    IndexAlreadyExists,
    /// An index definition cannot be built: no name, no keyword property, or a vector dimension
    /// of zero.
    InvalidIndexDefinition,
    /// A host procedure was registered in the `db` namespace, which built-in procedures reserve.
    ReservedProcedureName,
    /// A feature this version does not support.
    Unsupported,
    /// The storage engine failed.
    Storage,
    /// Stored data does not decode, breaks an invariant, or makes the storage engine fail. Once
    /// the storage engine panics on a damaged file, every later operation on that [`Database`]
    /// fails this way until the file is opened again.
    ///
    /// [`Database`]: crate::Database
    CorruptData,
    /// The database was written in a format version this build cannot read.
    UnsupportedFormatVersion,
    /// No node, relationship, or token identifiers are left.
    IdSpaceExhausted,
    /// The reader or writer the host passed to [`Database::import`] or [`Database::export`]
    /// failed.
    ///
    /// [`Database::import`]: crate::Database::import
    /// [`Database::export`]: crate::Database::export
    Io,
    /// A dump given to [`Database::import`](crate::Database::import) is malformed: a line that
    /// is not the JSON the format defines, lines out of order, an ID out of order, a
    /// relationship to a node the dump has not created, or a dump cut short before its end line.
    InvalidDump,
    /// A dump was written in a format version newer than this build reads.
    UnsupportedDumpVersion,
    /// A dump was imported into a database that holds an index or has ever held a node or a
    /// relationship.
    DatabaseNotEmpty,
}

impl ErrorKind {
    /// Whether the failure is decided by the types of its operands alone. Evaluating an
    /// expression that has no variables or parameters can only fail this way when the query text
    /// is wrong, so such a failure is reported before any data is read. Every other kind depends
    /// on values and is reported only when a row reaches the expression.
    pub(crate) fn is_type_error(self) -> bool {
        matches!(
            self,
            Self::InvalidArgumentType
                | Self::NonIntegerArgument
                | Self::TypeMismatch
                | Self::PropertyAccessOnNonEntity
                | Self::ListElementAccessByNonInteger
                | Self::MapElementAccessByNonString
        )
    }
}

/// A failed query or database operation.
#[derive(Debug)]
pub struct Error {
    kind: ErrorKind,
    message: String,
    source: Option<Box<dyn StdError + Send + Sync>>,
    rejects_type: bool,
}

impl Error {
    pub(crate) fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            source: None,
            rejects_type: false,
        }
    }

    /// A failure caused by `source`, such as an error a procedure body returned.
    pub(crate) fn caused_by(
        kind: ErrorKind,
        message: impl Into<String>,
        source: Box<dyn StdError + Send + Sync>,
    ) -> Self {
        Self {
            source: Some(source),
            ..Self::new(kind, message)
        }
    }

    /// A failure that rejects the type of a value but is reported as `kind`, such as a function
    /// argument of a type the function does not take.
    pub(crate) fn rejecting_type(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            rejects_type: true,
            ..Self::new(kind, message)
        }
    }

    /// Whether the failure is decided by the types of the values alone, as
    /// [`ErrorKind::is_type_error`] describes.
    pub(crate) fn is_type_error(&self) -> bool {
        self.rejects_type || self.kind.is_type_error()
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
            kind: if error.is_corrupt() {
                ErrorKind::CorruptData
            } else {
                ErrorKind::Storage
            },
            message: error.to_string(),
            source: Some(Box::new(error)),
            rejects_type: false,
        }
    }
}

impl From<GraphError> for Error {
    fn from(error: GraphError) -> Self {
        let kind = match &error {
            GraphError::Storage(error) if error.is_corrupt() => ErrorKind::CorruptData,
            GraphError::Storage(_) => ErrorKind::Storage,
            GraphError::Decode(_) | GraphError::Corrupt(_) => ErrorKind::CorruptData,
            GraphError::UnsupportedFormatVersion(_) => ErrorKind::UnsupportedFormatVersion,
            GraphError::IdSpaceExhausted => ErrorKind::IdSpaceExhausted,
            GraphError::NodeNotFound(_) | GraphError::RelationshipNotFound(_) => {
                ErrorKind::DeletedEntityAccess
            }
            GraphError::NodeHasRelationships(_) => ErrorKind::DeleteConnectedNode,
            GraphError::WriterOnThisThread => ErrorKind::WriteTransactionOnThisThread,
            GraphError::IndexNotFound(..) => ErrorKind::IndexNotFound,
            GraphError::IndexExists(_) => ErrorKind::IndexAlreadyExists,
            GraphError::InvalidIndex(_) => ErrorKind::InvalidIndexDefinition,
            GraphError::InvalidQueryVector(_) => ErrorKind::InvalidArgumentValue,
        };
        Self {
            kind,
            message: error.to_string(),
            source: Some(Box::new(error)),
            rejects_type: false,
        }
    }
}
