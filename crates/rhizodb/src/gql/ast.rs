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
    /// Statements joined by `NEXT`, each fed the rows the one before it returns. Holds two or
    /// more blocks, none of them a `Next`, and the first block has no `YIELD`.
    Next(Vec<Block>),
}

/// One statement of a `NEXT` chain: `NEXT YIELD a, b query`, where `yields` is `None` without
/// `YIELD`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Block {
    pub yields: Option<Vec<Name>>,
    pub query: Query,
}

/// Two or more single queries joined by one set operator.
///
/// `rest` is never empty. Mixing operators, or `UNION` and `UNION ALL`, is a syntax error.
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
    /// The rows of the first operand that the second lacks, as a multiset with `ALL`.
    Except {
        all: bool,
    },
    /// The rows in both operands, as a multiset with `ALL`.
    Intersect {
        all: bool,
    },
    /// The first operand's rows, or the second's when the first gives none.
    Otherwise,
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
    CallSubquery(Box<CallSubquery>),
}

/// `[OPTIONAL] CALL [(variables)] { query }`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CallSubquery {
    pub optional: bool,
    /// The variable scope clause, or `None` when the body imports with a leading `WITH`.
    pub imports: Option<Imports>,
    pub body: Query,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Imports {
    /// `(*)`
    All,
    /// `(a, b)`, possibly empty.
    Variables(Vec<Name>),
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
    /// Nulls last.
    #[default]
    Ascending,
    /// Nulls first.
    Descending,
    AscendingNullsFirst,
    DescendingNullsLast,
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
    /// `variable:A:B`, `variable:A&B`, or `variable IS A&B`
    Labels { variable: Name, labels: LabelExpr },
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum RemoveItem {
    Property { base: Expr, key: Name },
    Labels { variable: Name, labels: LabelExpr },
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
            Self::And(operands) => {
                let mut names = Vec::new();
                for operand in operands {
                    names.extend(operand.label_set()?);
                }
                Some(names)
            }
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

/// The orientations of edge a relationship pattern accepts, one variant per edge pattern form.
/// Cypher's `--` is `Any` and its `<-->` is `IncomingOrOutgoing`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Direction {
    /// `-[]->` and `->`.
    Outgoing,
    /// `<-[]-` and `<-`.
    Incoming,
    /// `~[]~` and `~`.
    Undirected,
    /// `<~[]~` and `<~`.
    IncomingOrUndirected,
    /// `~[]~>` and `~>`.
    UndirectedOrOutgoing,
    /// `<-[]->` and `<->`.
    IncomingOrOutgoing,
    /// `-[]-` and `-`.
    Any,
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
    Function(Box<FunctionCall>),
    CountStar,
    Case(Case),
    ListComprehension(ListComprehension),
    ListPredicate(ListPredicate),
    /// A pattern used as a boolean expression, as in `WHERE (a)-->(b)`.
    Pattern(Box<PatternElement>),
    PatternComprehension(Box<PatternComprehension>),
    Exists(Box<Subquery>),
    /// `COUNT { ... }`: how many rows the subquery gives.
    Count(Box<Subquery>),
    /// `COLLECT { query }`: the values of the single column the query returns.
    Collect(Box<SingleQuery>),
    /// `VALUE { query }`: the value of the single column in the one row the query returns.
    Value(Box<SingleQuery>),
    /// `operand IS [NOT] ...` for the tests after `IS` other than `NULL` and `LABELED`.
    Is(Box<IsTest>),
    /// `CAST(operand AS target)`.
    Cast(Box<Cast>),
    Reduce(Box<Reduce>),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct IsTest {
    pub operand: Expr,
    pub negated: bool,
    pub test: Test,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Test {
    /// `IS TYPED type` and `IS :: type`.
    Typed(TypeExpr),
    /// `IS TRUE`, `IS FALSE`, and `IS UNKNOWN` as `None`.
    Truth(Option<bool>),
    /// `IS [form] NORMALIZED`.
    Normalized(NormalForm),
    /// `IS DISTINCT FROM other`.
    DistinctFrom(Expr),
}

/// A Unicode normalization form.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum NormalForm {
    Nfc,
    Nfd,
    Nfkc,
    Nfkd,
}

impl NormalForm {
    /// The form named by `name`, in any letter case.
    pub(crate) fn named(name: &str) -> Option<Self> {
        [
            ("NFC", Self::Nfc),
            ("NFD", Self::Nfd),
            ("NFKC", Self::Nfkc),
            ("NFKD", Self::Nfkd),
        ]
        .into_iter()
        .find(|(held, _)| held.eq_ignore_ascii_case(name))
        .map(|(_, form)| form)
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Nfc => "NFC",
            Self::Nfd => "NFD",
            Self::Nfkc => "NFKC",
            Self::Nfkd => "NFKD",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Cast {
    pub operand: Expr,
    pub target: ValueType,
}

/// A union of value types, any of which a value may belong to. Never empty.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct TypeExpr {
    pub members: Vec<TypeMember>,
}

/// One type of a union. Null belongs to it unless `not_null` is set.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct TypeMember {
    pub value_type: ValueType,
    pub not_null: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum ValueType {
    /// Every value.
    Any,
    /// No value, not even null.
    Nothing,
    /// Only null.
    Null,
    Boolean,
    String,
    Integer,
    Float,
    Date,
    LocalTime,
    ZonedTime,
    LocalDateTime,
    ZonedDateTime,
    Duration,
    Bytes,
    Map,
    Node,
    Relationship,
    Path,
    /// A list whose every item belongs to the union.
    List(Box<TypeExpr>),
}

/// `reduce(accumulator = init, variable IN source | body)`
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Reduce {
    pub accumulator: Name,
    pub init: Expr,
    pub variable: Name,
    pub source: Expr,
    pub body: Expr,
}

/// `[path = pattern WHERE filter | projection]`
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PatternComprehension {
    pub path_variable: Option<Name>,
    pub element: PatternElement,
    pub filter: Option<Box<Expr>>,
    pub projection: Box<Expr>,
}

/// The body of `EXISTS { ... }` and `COUNT { ... }`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Subquery {
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
    /// GQL's `||`, on two strings or two lists.
    Concat,
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
    /// One condition in the searched form, and one or more tests of the scrutinee in the simple
    /// form, any of which takes the branch.
    pub when: Vec<WhenOperand>,
    pub then: Expr,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum WhenOperand {
    /// The condition of a searched branch, or a value the scrutinee must equal.
    Value(Expr),
    /// `WHEN < value` and the other comparisons.
    Compare(ComparisonOp, Expr),
    /// `WHEN IS NULL` and `WHEN IS NOT NULL`.
    IsNull { negated: bool },
}

impl WhenOperand {
    pub(crate) fn expr(&self) -> Option<&Expr> {
        match self {
            Self::Value(expr) | Self::Compare(_, expr) => Some(expr),
            Self::IsNull { .. } => None,
        }
    }
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
