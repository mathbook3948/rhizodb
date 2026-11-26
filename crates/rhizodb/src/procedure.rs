use std::collections::BTreeMap;
use std::error::Error as StdError;
use std::fmt;
use std::sync::Arc;

use crate::result::Value;

/// The rows a procedure body returns, each with a value per declared output in declaration
/// order. Collect it from an iterator of rows, or convert it from a `Vec` of rows.
///
/// ```
/// use rhizodb::ProcedureOutput;
/// use rhizodb::Value;
///
/// let collected: ProcedureOutput = (1..=2).map(|i| vec![Value::Integer(i)]).collect();
/// let mut pushed = ProcedureOutput::new();
/// pushed.push_row(vec![Value::Integer(1)]);
/// pushed.push_row(vec![Value::Integer(2)]);
/// assert_eq!(collected, pushed);
/// assert_eq!(pushed, ProcedureOutput::from(vec![vec![Value::Integer(1)], vec![Value::Integer(2)]]));
/// ```
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProcedureOutput {
    rows: Vec<Vec<Value>>,
}

impl ProcedureOutput {
    /// No rows.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `row` after the rows already there.
    pub fn push_row(&mut self, row: Vec<Value>) {
        self.rows.push(row);
    }

    pub(crate) fn into_rows(self) -> Vec<Vec<Value>> {
        self.rows
    }
}

impl From<Vec<Vec<Value>>> for ProcedureOutput {
    fn from(rows: Vec<Vec<Value>>) -> Self {
        Self { rows }
    }
}

impl FromIterator<Vec<Value>> for ProcedureOutput {
    fn from_iter<I: IntoIterator<Item = Vec<Value>>>(rows: I) -> Self {
        Self {
            rows: rows.into_iter().collect(),
        }
    }
}

/// Why a procedure body failed. A query reports it as
/// [`ErrorKind::ProcedureFailed`](crate::ErrorKind::ProcedureFailed), with this error as the
/// [`source`](std::error::Error::source) of the [`Error`](crate::Error).
///
/// ```
/// use rhizodb::Database;
/// use rhizodb::ErrorKind;
/// use rhizodb::Parameters;
/// use rhizodb::Procedure;
/// use rhizodb::ProcedureError;
/// use rhizodb::ProcedureOutput;
///
/// fn refuse(_: &[rhizodb::Value]) -> Result<ProcedureOutput, ProcedureError> {
///     Err("not today".into())
/// }
///
/// let db = Database::in_memory()?;
/// db.register_procedure(Procedure::new("host.refuse", refuse).output("x"))?;
/// let error = db.execute("CALL host.refuse() YIELD x RETURN x", &Parameters::new()).unwrap_err();
/// assert_eq!(error.kind(), ErrorKind::ProcedureFailed);
/// # Ok::<(), rhizodb::Error>(())
/// ```
pub type ProcedureError = Box<dyn StdError + Send + Sync>;

type Body = dyn Fn(&[Value]) -> Result<ProcedureOutput, ProcedureError> + Send + Sync;

/// The type a procedure parameter accepts. Every parameter also accepts null.
///
/// ```
/// use rhizodb::Database;
/// use rhizodb::Parameters;
/// use rhizodb::Procedure;
/// use rhizodb::Value;
/// use rhizodb::ValueType;
///
/// let db = Database::in_memory()?;
/// db.register_procedure(
///     Procedure::new("math.half", |args| match args.first() {
///         Some(Value::Float(x)) => Ok(vec![vec![Value::Float(x / 2.0)]].into()),
///         _ => Ok(vec![vec![Value::Null]].into()),
///     })
///     .parameter("x", ValueType::Float)
///     .output("half"),
/// )?;
/// let result = db.execute("CALL math.half(3) YIELD half RETURN half", &Parameters::new())?;
/// assert_eq!(result.rows(), [vec![Value::Float(1.5)]]);
/// assert!(db.execute("CALL math.half('3') YIELD half RETURN half", &Parameters::new()).is_err());
/// # Ok::<(), rhizodb::Error>(())
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ValueType {
    /// Any value.
    Any,
    /// A boolean.
    Boolean,
    /// An integer.
    Integer,
    /// A float. An integer argument is passed as the nearest float, as integer and float
    /// arithmetic converts it.
    Float,
    /// An integer or a float, passed as it is.
    Number,
    /// A string.
    String,
    /// A list.
    List,
    /// A map.
    Map,
    /// A node.
    Node,
    /// A relationship.
    Relationship,
    /// A path.
    Path,
}

/// The name, parameters, and outputs of a procedure, whether a host registered it or the engine
/// provides it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Signature {
    pub(crate) name: String,
    pub(crate) parameters: Vec<(String, ValueType)>,
    pub(crate) outputs: Vec<String>,
}

/// A named routine written in Rust that queries run with `CALL`.
///
/// A procedure sees only its arguments, never the graph. A query calls it once per incoming row
/// and gets one row per row the body returns. The `db` namespace is reserved for the procedures
/// the engine provides, such as `db.index.keyword.queryNodes`.
///
/// ```
/// use rhizodb::Database;
/// use rhizodb::Parameters;
/// use rhizodb::Procedure;
/// use rhizodb::ProcedureOutput;
/// use rhizodb::Value;
/// use rhizodb::ValueType;
///
/// let db = Database::in_memory()?;
/// db.register_procedure(
///     Procedure::new("text.words", |args| {
///         let Some(Value::String(text)) = args.first() else {
///             return Ok(ProcedureOutput::new());
///         };
///         Ok(text
///             .split_whitespace()
///             .map(|word| vec![Value::String(word.to_owned())])
///             .collect())
///     })
///     .parameter("text", ValueType::String)
///     .output("word"),
/// )?;
/// let result = db.execute(
///     "CALL text.words('graph database') YIELD word RETURN word",
///     &Parameters::new(),
/// )?;
/// assert_eq!(result.rows().len(), 2);
/// # Ok::<(), rhizodb::Error>(())
/// ```
#[derive(Clone)]
pub struct Procedure {
    pub(crate) signature: Signature,
    pub(crate) body: Arc<Body>,
}

impl Procedure {
    /// A procedure called as `name`, such as `text.words`, with no parameters and no outputs
    /// yet. `body` gets one argument per declared parameter.
    pub fn new(
        name: impl Into<String>,
        body: impl Fn(&[Value]) -> Result<ProcedureOutput, ProcedureError> + Send + Sync + 'static,
    ) -> Self {
        Self {
            signature: Signature {
                name: name.into(),
                parameters: Vec::new(),
                outputs: Vec::new(),
            },
            body: Arc::new(body),
        }
    }

    /// Declares the next parameter.
    #[must_use]
    pub fn parameter(mut self, name: impl Into<String>, accepts: ValueType) -> Self {
        self.signature.parameters.push((name.into(), accepts));
        self
    }

    /// Declares the next output column.
    #[must_use]
    pub fn output(mut self, name: impl Into<String>) -> Self {
        self.signature.outputs.push(name.into());
        self
    }
}

impl fmt::Debug for Procedure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Procedure")
            .field("name", &self.signature.name)
            .field("parameters", &self.signature.parameters)
            .field("outputs", &self.signature.outputs)
            .finish_non_exhaustive()
    }
}

/// The procedures one database can call, by name.
pub(crate) type Procedures = BTreeMap<String, Arc<Procedure>>;
