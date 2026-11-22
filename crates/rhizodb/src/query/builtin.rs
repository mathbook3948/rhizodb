use std::sync::LazyLock;

use crate::error::Error;
use crate::error::ErrorKind;
use crate::graph::GraphTx;
use crate::procedure::Signature;
use crate::procedure::ValueType;
use crate::storage::ReadTransaction;

use super::datum::Datum;

/// The namespace whose procedures the engine provides. A host cannot register a procedure in it.
pub(crate) const RESERVED_NAMESPACE: &str = "db";

/// A procedure the engine provides. Unlike a host procedure, it reads the graph of the
/// transaction that calls it, including that transaction's own writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Builtin {
    KeywordQuery,
    VectorQuery,
}

fn signature(name: &str, query: (&str, ValueType)) -> Signature {
    Signature {
        name: name.to_owned(),
        parameters: vec![
            ("index".to_owned(), ValueType::String),
            (query.0.to_owned(), query.1),
            ("limit".to_owned(), ValueType::Integer),
        ],
        outputs: vec!["node".to_owned(), "score".to_owned()],
    }
}

static KEYWORD_QUERY: LazyLock<Signature> =
    LazyLock::new(|| signature("db.index.keyword.query", ("query", ValueType::String)));

static VECTOR_QUERY: LazyLock<Signature> =
    LazyLock::new(|| signature("db.index.vector.query", ("vector", ValueType::List)));

/// Whether `name` lies in the namespace reserved for built-in procedures.
pub(crate) fn is_reserved(name: &str) -> bool {
    name.split('.').next() == Some(RESERVED_NAMESPACE)
}

impl Builtin {
    const ALL: [Builtin; 2] = [Builtin::KeywordQuery, Builtin::VectorQuery];

    pub(crate) fn named(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|builtin| builtin.signature().name == name)
    }

    pub(crate) fn signature(self) -> &'static Signature {
        match self {
            Self::KeywordQuery => &KEYWORD_QUERY,
            Self::VectorQuery => &VECTOR_QUERY,
        }
    }

    /// Runs the procedure on arguments that already match its parameters. A null argument gives
    /// no rows.
    pub(crate) fn call<T: ReadTransaction>(
        self,
        tx: &GraphTx<T>,
        args: &[Datum],
    ) -> Result<Vec<Vec<Datum>>, Error> {
        if args.iter().any(Datum::is_null) {
            return Ok(Vec::new());
        }
        let [Datum::String(index), query, Datum::Integer(limit)] = args else {
            return Ok(Vec::new());
        };
        let name = &self.signature().name;
        let limit = usize::try_from(*limit).map_err(|_| {
            Error::new(
                ErrorKind::InvalidArgumentValue,
                format!("the limit of `{name}` cannot be negative"),
            )
        })?;
        let found = match (self, query) {
            (Self::KeywordQuery, Datum::String(text)) => tx.keyword_search(index, text, limit)?,
            (Self::VectorQuery, Datum::List(items)) => {
                let vector = items
                    .iter()
                    .map(|item| match item {
                        Datum::Integer(value) => Some(*value as f64),
                        Datum::Float(value) => Some(*value),
                        _ => None,
                    })
                    .collect::<Option<Vec<f64>>>()
                    .ok_or_else(|| {
                        Error::new(
                            ErrorKind::InvalidArgumentValue,
                            format!("the vector of `{name}` must hold only numbers"),
                        )
                    })?;
                tx.vector_search(index, &vector, limit)?
            }
            _ => return Ok(Vec::new()),
        };
        Ok(found
            .into_iter()
            .map(|(node, score)| vec![Datum::Node(node), Datum::Float(score)])
            .collect())
    }
}
