/// An identifier: a variable, label, relationship type, property key, or parameter name.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Name(pub String);

impl Name {
    pub(crate) fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }
}

/// A byte range in the query text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Span {
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Query {
    Single(SingleQuery),
    Union(Union),
}

/// Two or more single queries joined by one kind of `UNION`.
///
/// `rest` is never empty. Mixing `UNION` and `UNION ALL` is a syntax error.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Union {
    pub kind: UnionKind,
    pub first: SingleQuery,
    pub rest: Vec<SingleQuery>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum UnionKind {
    Distinct,
    All,
}

/// Clauses in source order. Never empty.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SingleQuery {
    pub clauses: Vec<Clause>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Clause {
    Match(Match),
    Merge(Merge),
    /// `INSERT` and Cypher's `CREATE`.
    Insert(Vec<PatternPart>),
    With(With),
    Return(Projection),
    Unwind(Unwind),
    Set(Vec<SetItem>),
    Remove(Vec<RemoveItem>),
    Delete(Delete),
    Call(Call),
    /// `FILTER condition` or `FILTER WHERE condition`.
    Filter(Expr),
    /// `LET a = value, b = value`. Never empty.
    Let(Vec<(Name, Expr)>),
}

/// `CALL name(args) YIELD items WHERE filter`. `args` is `None` when the parentheses are left
/// out, and `yields` is `None` without `YIELD`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Call {
    pub name: Vec<Name>,
    pub args: Option<Vec<Expr>>,
    pub yields: Option<Yield>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Yield {
    /// `YIELD *`, which only a query that is a single `CALL` accepts.
    Star,
    Items {
        items: Vec<YieldItem>,
        filter: Option<Expr>,
    },
}

/// `output AS alias`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct YieldItem {
    pub output: Name,
    pub alias: Option<Name>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Match {
    pub optional: bool,
    pub patterns: Vec<PatternPart>,
    pub filter: Option<Expr>,
}

/// `MERGE pattern ON CREATE SET ... ON MATCH SET ...`. Repeated `ON` actions of
/// one kind are concatenated in source order.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Merge {
    pub pattern: PatternPart,
    pub on_create: Vec<SetItem>,
    pub on_match: Vec<SetItem>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct With {
    pub projection: Projection,
    pub filter: Option<Expr>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Projection {
    pub distinct: bool,
    pub items: ProjectionItems,
    pub order_by: Vec<SortItem>,
    /// From `SKIP` or `OFFSET`.
    pub skip: Option<Expr>,
    pub limit: Option<Expr>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ProjectionItems {
    /// `*`, optionally followed by extra items. Never empty on its own.
    Star(Vec<ProjectionItem>),
    /// One or more explicit items.
    Items(Vec<ProjectionItem>),
}

/// One projected expression. `span` covers the expression text, which is the
/// column name when there is no alias.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ProjectionItem {
    pub expr: Expr,
    pub alias: Option<Name>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SortItem {
    pub expr: Expr,
    pub direction: SortDirection,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub(crate) enum SortDirection {
    #[default]
    Ascending,
    Descending,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Unwind {
    pub expr: Expr,
    pub variable: Name,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Delete {
    pub detach: bool,
    pub targets: Vec<Expr>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum SetItem {
    /// `base.key = value`
    Property { base: Expr, key: Name, value: Expr },
    /// `variable = map`
    ReplaceProperties { variable: Name, value: Expr },
    /// `variable += map`
    MergeProperties { variable: Name, value: Expr },
    /// `variable:A:B`
    Labels { variable: Name, labels: Vec<Name> },
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum RemoveItem {
    Property { base: Expr, key: Name },
    Labels { variable: Name, labels: Vec<Name> },
}

/// A comma-separated pattern, optionally bound to a path variable.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PatternPart {
    pub path_variable: Option<Name>,
    pub element: PatternElement,
}

/// A node followed by zero or more relationship-and-node steps.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PatternElement {
    pub start: NodePattern,
    pub steps: Vec<PatternStep>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PatternStep {
    pub relationship: RelationshipPattern,
    pub node: NodePattern,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct NodePattern {
    pub variable: Option<Name>,
    /// `None` accepts any node.
    pub labels: Option<LabelExpr>,
    pub properties: Option<Properties>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RelationshipPattern {
    pub variable: Option<Name>,
    /// A test on the type, which is the only label of a relationship. `None` accepts any type.
    pub labels: Option<LabelExpr>,
    pub length: Option<Length>,
    pub properties: Option<Properties>,
    pub direction: Direction,
}

/// A label expression. Cypher's `:A:B` is the conjunction of `A` and `B`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum LabelExpr {
    Name(Name),
    /// `%`, which an element with at least one label satisfies.
    Wildcard,
    Not(Box<LabelExpr>),
    /// Two or more operands.
    And(Vec<LabelExpr>),
    /// Two or more operands.
    Or(Vec<LabelExpr>),
}

impl LabelExpr {
    /// Whether an element satisfies the expression, where `labeled` says whether it has any
    /// label and `has` whether it has the named one.
    pub(crate) fn matches<E>(
        &self,
        labeled: bool,
        has: &mut impl FnMut(&str) -> Result<bool, E>,
    ) -> Result<bool, E> {
        Ok(match self {
            Self::Name(name) => has(&name.0)?,
            Self::Wildcard => labeled,
            Self::Not(operand) => !operand.matches(labeled, has)?,
            Self::And(operands) => {
                for operand in operands {
                    if !operand.matches(labeled, has)? {
                        return Ok(false);
                    }
                }
                true
            }
            Self::Or(operands) => {
                for operand in operands {
                    if operand.matches(labeled, has)? {
                        return Ok(true);
                    }
                }
                false
            }
        })
    }

    /// Every label the expression names, in source order and with repeats.
    pub(crate) fn names(&self) -> Vec<&Name> {
        match self {
            Self::Name(name) => vec![name],
            Self::Wildcard => Vec::new(),
            Self::Not(operand) => operand.names(),
            Self::And(operands) | Self::Or(operands) => {
                operands.iter().flat_map(Self::names).collect()
            }
        }
    }

    /// A label that every element satisfying the expression has, if the expression requires one.
    pub(crate) fn required(&self) -> Option<&Name> {
        match self {
            Self::Name(name) => Some(name),
            Self::And(operands) => operands.iter().find_map(Self::required),
            Self::Wildcard | Self::Not(_) | Self::Or(_) => None,
        }
    }

    /// The labels of a name or a conjunction of names, the only forms a pattern that creates
    /// accepts.
    pub(crate) fn label_set(&self) -> Option<Vec<&Name>> {
        match self {
            Self::Name(name) => Some(vec![name]),
            Self::And(operands) => operands
                .iter()
                .map(|operand| match operand {
                    Self::Name(name) => Some(name),
                    _ => None,
                })
                .collect(),
            Self::Wildcard | Self::Not(_) | Self::Or(_) => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Properties {
    Map(Vec<(Name, Expr)>),
    Parameter(Name),
}

/// Variable-length bounds. `*` is both `None`, `*3` is both `Some(3)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Length {
    pub min: Option<u64>,
    pub max: Option<u64>,
}

/// `<-->` is parsed as `Undirected`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Direction {
    Outgoing,
    Incoming,
    Undirected,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Expr {
    Literal(Literal),
    Parameter(Name),
    Variable(Name),
    List(Vec<Expr>),
    Map(Vec<(Name, Expr)>),
    Property {
        base: Box<Expr>,
        key: Name,
    },
    Index {
        base: Box<Expr>,
        index: Box<Expr>,
    },
    Slice {
        base: Box<Expr>,
        from: Option<Box<Expr>>,
        to: Option<Box<Expr>>,
    },
    HasLabels {
        operand: Box<Expr>,
        labels: LabelExpr,
    },
    Unary {
        op: UnaryOp,
        operand: Box<Expr>,
    },
    Binary {
        op: BinaryOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    /// `a < b <= c` is one chain meaning `a < b AND b <= c`. `rest` is never empty.
    Comparison {
        first: Box<Expr>,
        rest: Vec<(ComparisonOp, Expr)>,
    },
    IsNull {
        operand: Box<Expr>,
        negated: bool,
    },
    Function(FunctionCall),
    CountStar,
    Case(Case),
    ListComprehension(ListComprehension),
    ListPredicate(ListPredicate),
    /// A pattern used as a boolean expression, as in `WHERE (a)-->(b)`.
    Pattern(PatternElement),
    PatternComprehension(PatternComprehension),
    Exists(Box<ExistsSubquery>),
}

/// `[path = pattern WHERE filter | projection]`
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PatternComprehension {
    pub path_variable: Option<Name>,
    pub element: PatternElement,
    pub filter: Option<Box<Expr>>,
    pub projection: Box<Expr>,
}

/// The body of `EXISTS { ... }`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ExistsSubquery {
    Pattern {
        patterns: Vec<PatternPart>,
        filter: Option<Expr>,
    },
    Query(SingleQuery),
}

/// A unary minus directly in front of a number is folded into the literal.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Literal {
    Integer(i64),
    Float(f64),
    String(String),
    Boolean(bool),
    Null,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum UnaryOp {
    Negate,
    Plus,
    Not,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum BinaryOp {
    Add,
    Subtract,
    Multiply,
    Divide,
    Modulo,
    Power,
    And,
    Or,
    Xor,
    In,
    StartsWith,
    EndsWith,
    Contains,
    RegexMatch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum ComparisonOp {
    Equal,
    NotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FunctionCall {
    pub namespace: Vec<Name>,
    pub name: Name,
    pub distinct: bool,
    pub args: Vec<Expr>,
}

/// `scrutinee` is `Some` for the simple form `CASE x WHEN 1 THEN ...`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Case {
    pub scrutinee: Option<Box<Expr>>,
    pub branches: Vec<WhenThen>,
    pub otherwise: Option<Box<Expr>>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct WhenThen {
    pub when: Expr,
    pub then: Expr,
}

/// `[variable IN source WHERE filter | projection]`
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ListComprehension {
    pub variable: Name,
    pub source: Box<Expr>,
    pub filter: Option<Box<Expr>>,
    pub projection: Option<Box<Expr>>,
}

/// `all(x IN list WHERE predicate)` and its siblings.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ListPredicate {
    pub quantifier: Quantifier,
    pub variable: Name,
    pub source: Box<Expr>,
    pub predicate: Box<Expr>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Quantifier {
    All,
    Any,
    None,
    Single,
}

#[cfg(test)]
#[path = "ast.test.rs"]
mod tests;
