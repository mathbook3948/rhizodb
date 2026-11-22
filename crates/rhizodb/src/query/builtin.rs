use std::sync::LazyLock;

use crate::error::Error;
use crate::error::ErrorKind;
use crate::graph::ElementKind;
use crate::graph::GraphTx;
use crate::graph::NodeId;
use crate::graph::RelationshipId;
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
    KeywordNodes,
    KeywordRelationships,
    VectorNodes,
    VectorRelationships,
}

const KEYWORD: (&str, ValueType) = ("query", ValueType::String);
const VECTOR: (&str, ValueType) = ("vector", ValueType::List);

fn signature(name: &str, query: (&str, ValueType), element: &str) -> Signature {
    Signature {
        name: name.to_owned(),
        parameters: vec![
            ("index".to_owned(), ValueType::String),
            (query.0.to_owned(), query.1),
            ("limit".to_owned(), ValueType::Integer),
        ],
        outputs: vec![element.to_owned(), "score".to_owned()],
    }
}

static KEYWORD_NODES: LazyLock<Signature> =
    LazyLock::new(|| signature("db.index.keyword.query", KEYWORD, "node"));

static KEYWORD_RELATIONSHIPS: LazyLock<Signature> = LazyLock::new(|| {
    signature(
        "db.index.keyword.queryRelationships",
        KEYWORD,
        "relationship",
    )
});

static VECTOR_NODES: LazyLock<Signature> =
    LazyLock::new(|| signature("db.index.vector.query", VECTOR, "node"));

static VECTOR_RELATIONSHIPS: LazyLock<Signature> =
    LazyLock::new(|| signature("db.index.vector.queryRelationships", VECTOR, "relationship"));

/// Whether `name` lies in the namespace reserved for built-in procedures.
pub(crate) fn is_reserved(name: &str) -> bool {
    name.split('.').next() == Some(RESERVED_NAMESPACE)
}

impl Builtin {
    const ALL: [Builtin; 4] = [
        Builtin::KeywordNodes,
        Builtin::KeywordRelationships,
        Builtin::VectorNodes,
        Builtin::VectorRelationships,
    ];

    pub(crate) fn named(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|builtin| builtin.signature().name == name)
    }

    pub(crate) fn signature(self) -> &'static Signature {
        match self {
            Self::KeywordNodes => &KEYWORD_NODES,
            Self::KeywordRelationships => &KEYWORD_RELATIONSHIPS,
            Self::VectorNodes => &VECTOR_NODES,
            Self::VectorRelationships => &VECTOR_RELATIONSHIPS,
        }
    }

    fn element(self) -> ElementKind {
        match self {
            Self::KeywordNodes | Self::VectorNodes => ElementKind::Node,
            Self::KeywordRelationships | Self::VectorRelationships => ElementKind::Relationship,
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
        let element = self.element();
        let found = match (self, query) {
            (Self::KeywordNodes | Self::KeywordRelationships, Datum::String(text)) => {
                tx.keyword_search(index, element, text, limit)?
            }
            (Self::VectorNodes | Self::VectorRelationships, Datum::List(items)) => {
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
                tx.vector_search(index, element, &vector, limit)?
            }
            _ => return Ok(Vec::new()),
        };
        Ok(found
            .into_iter()
            .map(|(id, score)| {
                let found = match element {
                    ElementKind::Node => Datum::Node(NodeId(id)),
                    ElementKind::Relationship => Datum::Relationship(RelationshipId(id)),
                };
                vec![found, Datum::Float(score)]
            })
            .collect())
    }
}
