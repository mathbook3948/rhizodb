use crate::gql::ast::BinaryOp;
use crate::gql::ast::ComparisonOp;
use crate::gql::ast::Direction;
use crate::gql::ast::Quantifier;
use crate::gql::ast::SortDirection;

use super::datum::Datum;
use super::functions::Function;

/// The index of a variable's value in a row. Every row of a query has one entry per slot.
pub(crate) type Slot = usize;

/// A bound query: read-only plans can only hold read clauses, so they run on a read transaction.
#[derive(Clone, Debug)]
pub(crate) enum Plan {
    ReadOnly(Program<ReadClause>),
    Updating(Program<Clause>),
}

/// One or more pipelines whose results are concatenated, as `UNION` does.
#[derive(Clone, Debug)]
pub(crate) struct Program<C> {
    pub(crate) parts: Vec<Pipeline<C>>,
    pub(crate) distinct: bool,
    pub(crate) columns: Vec<String>,
    pub(crate) slot_count: usize,
    /// The unconditionally evaluated expressions that read no row, no parameter, and match no
    /// pattern. Each is evaluated once before the first clause runs, so a type error in the
    /// query text is reported whatever the data.
    pub(crate) closed: Vec<Expr>,
}

/// Clauses applied in order to the rows, then the slots read out as the result columns.
#[derive(Clone, Debug)]
pub(crate) struct Pipeline<C> {
    pub(crate) clauses: Vec<C>,
    pub(crate) output: Vec<Slot>,
}

#[derive(Clone, Debug)]
pub(crate) enum Clause {
    Read(ReadClause),
    Update(UpdateClause),
}

#[derive(Clone, Debug)]
pub(crate) enum ReadClause {
    Match(MatchClause),
    Unwind { expr: Expr, slot: Slot },
    Project(Box<Projection>),
}

#[derive(Clone, Debug)]
pub(crate) enum UpdateClause {
    Create(Vec<PatternPart>),
    Merge(Box<MergeClause>),
    Set(Vec<SetAction>),
    Remove(Vec<RemoveAction>),
    Delete { detach: bool, targets: Vec<Expr> },
}

#[derive(Clone, Debug)]
pub(crate) struct MatchClause {
    pub(crate) optional: bool,
    pub(crate) patterns: Vec<PatternPart>,
    pub(crate) filter: Option<Expr>,
    /// Slots the clause binds, set to null when an optional match finds nothing.
    pub(crate) introduced: Vec<Slot>,
}

#[derive(Clone, Debug)]
pub(crate) struct MergeClause {
    pub(crate) pattern: PatternPart,
    pub(crate) on_create: Vec<SetAction>,
    pub(crate) on_match: Vec<SetAction>,
}

#[derive(Clone, Debug)]
pub(crate) struct PatternPart {
    pub(crate) path: Option<Slot>,
    pub(crate) start: NodePattern,
    pub(crate) steps: Vec<Step>,
}

#[derive(Clone, Debug)]
pub(crate) struct Step {
    pub(crate) relationship: RelationshipPattern,
    pub(crate) node: NodePattern,
}

/// `bound` means the slot already holds this element when the pattern element is reached.
#[derive(Clone, Debug)]
pub(crate) struct NodePattern {
    pub(crate) slot: Slot,
    pub(crate) bound: bool,
    pub(crate) labels: Vec<String>,
    pub(crate) properties: Option<Expr>,
}

#[derive(Clone, Debug)]
pub(crate) struct RelationshipPattern {
    pub(crate) slot: Slot,
    pub(crate) bound: bool,
    pub(crate) types: Vec<String>,
    pub(crate) direction: Direction,
    pub(crate) length: Option<Hops>,
    pub(crate) properties: Option<Expr>,
}

/// Bounds of a variable-length relationship. `max` of `None` is unbounded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Hops {
    pub(crate) min: u64,
    pub(crate) max: Option<u64>,
}

#[derive(Clone, Debug)]
pub(crate) enum SetAction {
    Property {
        target: Expr,
        key: String,
        value: Expr,
    },
    Properties {
        target: Slot,
        value: Expr,
        replace: bool,
    },
    Labels {
        target: Slot,
        labels: Vec<String>,
    },
}

#[derive(Clone, Debug)]
pub(crate) enum RemoveAction {
    Property { target: Expr, key: String },
    Labels { target: Slot, labels: Vec<String> },
}

#[derive(Clone, Debug)]
pub(crate) struct Projection {
    pub(crate) items: Items,
    pub(crate) distinct: bool,
    /// The slots that make up the projected row, for `DISTINCT` and for `UNION`.
    pub(crate) outputs: Vec<Slot>,
    pub(crate) order_by: Vec<(Expr, SortDirection)>,
    pub(crate) skip: Option<Expr>,
    pub(crate) limit: Option<Expr>,
    pub(crate) filter: Option<Expr>,
}

#[derive(Clone, Debug)]
pub(crate) enum Items {
    /// Each row is extended with the projected values.
    Plain(Vec<(Expr, Slot)>),
    /// Rows are grouped by `keys`, each group yields one row with the aggregates filled in,
    /// then `finals` are computed from the key and aggregate slots.
    Aggregate {
        keys: Vec<(Expr, Slot)>,
        aggregates: Vec<(Aggregate, Slot)>,
        finals: Vec<(Expr, Slot)>,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct Aggregate {
    pub(crate) function: AggregateFunction,
    pub(crate) distinct: bool,
    /// `None` only for `count(*)`.
    pub(crate) argument: Option<Expr>,
    pub(crate) extra: Option<Expr>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AggregateFunction {
    Count,
    Sum,
    Avg,
    Min,
    Max,
    Collect,
    StDev,
    StDevP,
    PercentileCont,
    PercentileDisc,
}

#[derive(Clone, Debug)]
pub(crate) enum Expr {
    Constant(Datum),
    /// A supplied parameter's value. It is data, not query text, so it never makes an
    /// expression closed.
    Parameter(Datum),
    Slot(Slot),
    List(Vec<Expr>),
    Map(Vec<(String, Expr)>),
    Property(Box<Expr>, String),
    Index(Box<Expr>, Box<Expr>),
    Slice(Box<Expr>, Option<Box<Expr>>, Option<Box<Expr>>),
    HasLabels(Box<Expr>, Vec<String>),
    Not(Box<Expr>),
    Negate(Box<Expr>),
    UnaryPlus(Box<Expr>),
    Binary(BinaryOp, Box<Expr>, Box<Expr>),
    Comparison(Box<Expr>, Vec<(ComparisonOp, Expr)>),
    IsNull {
        operand: Box<Expr>,
        negated: bool,
    },
    Call(Function, Vec<Expr>),
    Case(Box<Case>),
    ListComprehension(Box<ListComprehension>),
    Quantified(Box<Quantified>),
    /// A pattern used as a predicate: true when it matches at least once.
    Exists(Box<PatternQuery>),
    /// `[pattern WHERE filter | projection]`: one value per match.
    PatternComprehension(Box<PatternQuery>, Box<Expr>),
}

#[derive(Clone, Debug)]
pub(crate) struct Case {
    pub(crate) scrutinee: Option<Expr>,
    pub(crate) branches: Vec<(Expr, Expr)>,
    pub(crate) otherwise: Option<Expr>,
}

#[derive(Clone, Debug)]
pub(crate) struct ListComprehension {
    pub(crate) slot: Slot,
    pub(crate) source: Expr,
    pub(crate) filter: Option<Expr>,
    pub(crate) projection: Option<Expr>,
}

#[derive(Clone, Debug)]
pub(crate) struct Quantified {
    pub(crate) quantifier: Quantifier,
    pub(crate) slot: Slot,
    pub(crate) source: Expr,
    pub(crate) predicate: Expr,
}

/// Patterns matched from the current row inside an expression.
#[derive(Clone, Debug)]
pub(crate) struct PatternQuery {
    pub(crate) patterns: Vec<PatternPart>,
    pub(crate) filter: Option<Expr>,
}
