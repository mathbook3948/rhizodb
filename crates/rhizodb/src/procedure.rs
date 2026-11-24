use std::collections::BTreeMap;
use std::error::Error as StdError;
use std::fmt;
use std::sync::Arc;

use crate::result::Value;

/// What a procedure body returns: one row per result, each with a value per declared output in
/// declaration order, or the reason it failed, which a query reports as
/// [`ErrorKind::ProcedureFailed`](crate::ErrorKind::ProcedureFailed).
///
/// ```
/// use rhizodb::Database;
/// use rhizodb::ErrorKind;
/// use rhizodb::Parameters;
/// use rhizodb::Procedure;
/// use rhizodb::ProcedureOutput;
///
/// fn refuse(_: &[rhizodb::Value]) -> ProcedureOutput {
///     Err("not today".into())
/// }
///
/// let mut db = Database::in_memory()?;
/// db.register_procedure(Procedure::new("host.refuse", refuse).output("x"))?;
/// let error = db.execute("CALL host.refuse() YIELD x RETURN x", &Parameters::new()).unwrap_err();
/// assert_eq!(error.kind(), ErrorKind::ProcedureFailed);
/// # Ok::<(), rhizodb::Error>(())
/// ```
pub type ProcedureOutput = Result<Vec<Vec<Value>>, Box<dyn StdError + Send + Sync>>;

type Body = dyn Fn(&[Value]) -> ProcedureOutput + Send + Sync;

/// The type a procedure parameter accepts. Every parameter also accepts null.
///
/// ```
/// use rhizodb::Database;
/// use rhizodb::Parameters;
/// use rhizodb::Procedure;
/// use rhizodb::Value;
/// use rhizodb::ValueType;
///
/// let mut db = Database::in_memory()?;
/// db.register_procedure(
///     Procedure::new("math.half", |args| match args.first() {
///         Some(Value::Float(x)) => Ok(vec![vec![Value::Float(x / 2.0)]]),
///         _ => Ok(vec![vec![Value::Null]]),
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
    /// A float. An integer argument is passed as the equal float.
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
/// use rhizodb::Value;
/// use rhizodb::ValueType;
///
/// let mut db = Database::in_memory()?;
/// db.register_procedure(
///     Procedure::new("text.words", |args| {
///         let Some(Value::String(text)) = args.first() else {
///             return Ok(Vec::new());
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
        body: impl Fn(&[Value]) -> ProcedureOutput + Send + Sync + 'static,
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
    pub fn parameter(mut self, name: impl Into<String>, accepts: ValueType) -> Self {
        self.signature.parameters.push((name.into(), accepts));
        self
    }

    /// Declares the next output column.
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
