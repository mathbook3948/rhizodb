use std::cell::Cell;
use std::cell::RefCell;
use std::collections::BTreeMap;

use std::convert::Infallible;
use std::sync::Arc;

use crate::error::Error;
use crate::error::ErrorKind;
use crate::gql::ast::BinaryOp;
use crate::gql::ast::ComparisonOp;
use crate::gql::ast::LabelExpr;
use crate::gql::ast::NormalForm;
use crate::gql::ast::Quantifier;
use crate::gql::ast::TypeExpr;
use crate::gql::ast::ValueType;
use crate::graph::AdjacencyReader;
use crate::graph::ColumnReader;
use crate::graph::GraphTx;
use crate::graph::Label;
use crate::graph::LabelReader;
use crate::graph::NodeId;
use crate::graph::NodeRecord;
use crate::graph::PropertyKey;
use crate::graph::RelationshipId;
use crate::graph::RelationshipRecord;
use crate::graph::RelationshipType;
use crate::graph::Target;
use crate::graph::Token;
use crate::storage::ReadTransaction;
use crate::temporal::Clocks;

use super::compare::Comparison;
use super::compare::Key;
use super::compare::compare;
use super::compare::equals;
use super::datum;
use super::datum::Datum;
use super::functions;
use super::functions::Function;
use super::parallel::Morsel;
use super::plan::AggregateFunction;
use super::plan::Case;
use super::plan::Expr;
use super::plan::ListComprehension;
use super::plan::PatternQuery;
use super::plan::Quantified;
use super::plan::ReadClause;
use super::plan::Reduce;
use super::plan::Slot;
use super::plan::SubqueryKind;
use super::regex::Patterns;
use super::taint;
use super::taint::Taint;
use super::temporals;
use super::types;
use super::update::Graveyard;

/// One row: a value for every slot of the query.
pub(crate) type Row = Vec<Datum>;

/// A property index looked up: what it covers, its property, and its ID or none.
pub(crate) type IndexLookup = (Target, Token<PropertyKey>, Option<u32>);

/// Evaluates expressions and matches patterns against one transaction's view of the graph.
pub(crate) struct Env<'a, T: ReadTransaction> {
    pub(crate) tx: &'a GraphTx<T>,
    /// How many slots each row of the query has.
    pub(crate) slots: usize,
    /// The values of [`Program::parameters`](super::plan::Program::parameters), in order.
    pub(crate) parameters: &'a [Datum],
    /// The statement and transaction instants, which the clock functions other than `realtime`
    /// read.
    pub(crate) now: &'a Clocks,
    /// What the query deleted so far.
    pub(crate) graveyard: &'a Graveyard,
    /// The regular expressions the query compiled so far.
    pub(crate) patterns: &'a Patterns,
    /// Set when evaluation only looks for the type errors the query text fixes.
    pub(crate) check: Option<&'a Check>,
    /// The property columns read so far.
    pub(crate) columns: RefCell<ColumnReader>,
    /// The property keys looked up so far that have a token.
    pub(crate) keys: RefCell<Vec<(String, Token<PropertyKey>)>>,
    /// The reader of node labels, opened on the first read.
    pub(crate) labels: RefCell<LabelReader>,
    /// The labels looked up so far that have a token.
    pub(crate) label_tokens: RefCell<Vec<(String, Token<Label>)>>,
    /// The property indexes looked up so far, each with its ID or none.
    pub(crate) property_indexes: RefCell<Vec<IndexLookup>>,
    /// The adjacency segments read last.
    pub(crate) adjacency: RefCell<AdjacencyReader>,
    /// The relationship types looked up so far that have a token.
    pub(crate) rel_type_tokens: RefCell<Vec<(String, Token<RelationshipType>)>>,
    /// Set on a worker of a parallel read, which starts its first pattern from these nodes only.
    pub(crate) morsel: Option<&'a Morsel<'a>>,
}

impl<'a, T: ReadTransaction> Env<'a, T> {
    pub(crate) fn new(
        tx: &'a GraphTx<T>,
        slots: usize,
        parameters: &'a [Datum],
        now: &'a Clocks,
        graveyard: &'a Graveyard,
        patterns: &'a Patterns,
    ) -> Self {
        Self {
            tx,
            slots,
            parameters,
            now,
            graveyard,
            patterns,
            check: None,
            columns: RefCell::default(),
            keys: RefCell::default(),
            labels: RefCell::default(),
            label_tokens: RefCell::default(),
            property_indexes: RefCell::default(),
            adjacency: RefCell::default(),
            rel_type_tokens: RefCell::default(),
            morsel: None,
        }
    }
}

/// The state of an evaluation that only looks for the type errors the query text fixes. A step
/// that fails with a value error gives null instead, so the steps after it are still checked, and
/// each value carries a [`Taint`] that marks the parts read as null this way. A type error counts
/// only when no such part decides it.
#[derive(Default)]
pub(crate) struct Check {
    bound: Cell<Vec<(Slot, Taint)>>,
}

pub(crate) fn type_error(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::TypeMismatch, message)
}

fn deleted() -> Error {
    Error::new(
        ErrorKind::DeletedEntityAccess,
        "the element was deleted earlier in the query",
    )
}

/// Reads a value as a boolean for three-valued logic. `None` is null.
pub(crate) fn truth(value: &Datum) -> Result<Option<bool>, Error> {
    match value {
        Datum::Boolean(value) => Ok(Some(*value)),
        Datum::Null => Ok(None),
        other => Err(type_error(format!(
            "expected a boolean, got a {}",
            other.type_name()
        ))),
    }
}

fn from_truth(value: Option<bool>) -> Datum {
    value.map_or(Datum::Null, Datum::Boolean)
}

fn overflow() -> Error {
    Error::new(ErrorKind::IntegerOverflow, "integer arithmetic overflowed")
}

impl<T: ReadTransaction> Env<'_, T> {
    pub(crate) fn node(&self, id: NodeId) -> Result<NodeRecord, Error> {
        let labels = self
            .with_labels(id, |labels| labels.iter().copied().collect())?
            .ok_or_else(deleted)?;
        let properties = self.node_properties(id)?.ok_or_else(deleted)?;
        Ok(NodeRecord { labels, properties })
    }

    /// Calls `read` with the labels of the node with ID `id`, if there is such a node. A read
    /// started inside `read` looks the labels up on its own.
    pub(crate) fn with_labels<R>(
        &self,
        id: NodeId,
        read: impl FnOnce(&[Token<Label>]) -> R,
    ) -> Result<Option<R>, Error> {
        let Ok(mut reader) = self.labels.try_borrow_mut() else {
            return Ok(self.tx.node_labels(id)?.map(|labels| read(&labels)));
        };
        Ok(self.tx.with_labels(&mut reader, id, read)?)
    }

    pub(crate) fn label_token(&self, name: &str) -> Result<Option<Token<Label>>, Error> {
        if let Some((_, token)) = self
            .label_tokens
            .borrow()
            .iter()
            .find(|(held, _)| held == name)
        {
            return Ok(Some(*token));
        }
        let token = self.tx.lookup_token::<Label>(name)?;
        if let Some(token) = token {
            self.label_tokens
                .borrow_mut()
                .push((name.to_owned(), token));
        }
        Ok(token)
    }

    pub(crate) fn rel_type_token(
        &self,
        name: &str,
    ) -> Result<Option<Token<RelationshipType>>, Error> {
        if let Some((_, token)) = self
            .rel_type_tokens
            .borrow()
            .iter()
            .find(|(held, _)| held == name)
        {
            return Ok(Some(*token));
        }
        let token = self.tx.lookup_token::<RelationshipType>(name)?;
        if let Some(token) = token {
            self.rel_type_tokens
                .borrow_mut()
                .push((name.to_owned(), token));
        }
        Ok(token)
    }

    /// The ID of the property index over `property` of the elements `target` covers, if any.
    pub(crate) fn property_index(
        &self,
        target: Target,
        property: Token<PropertyKey>,
    ) -> Result<Option<u32>, Error> {
        if let Some((_, _, index)) = self
            .property_indexes
            .borrow()
            .iter()
            .find(|(held, key, _)| *held == target && *key == property)
        {
            return Ok(*index);
        }
        let index = self.tx.property_index(target, property)?;
        self.property_indexes
            .borrow_mut()
            .push((target, property, index));
        Ok(index)
    }

    /// One property of the node with ID `id`, or `None` when there is no such node. A `key` of
    /// `None` names a property no node holds.
    pub(crate) fn node_property(
        &self,
        id: NodeId,
        key: Option<Token<PropertyKey>>,
    ) -> Result<Option<Option<crate::value::PropertyValue>>, Error> {
        let (Ok(mut labels), Ok(mut columns)) =
            (self.labels.try_borrow_mut(), self.columns.try_borrow_mut())
        else {
            return Ok(self.tx.node_property(
                &mut LabelReader::default(),
                &mut ColumnReader::default(),
                id,
                key,
            )?);
        };
        Ok(self.tx.node_property(&mut labels, &mut columns, id, key)?)
    }

    /// Every property of the node with ID `id`, or `None` when there is no such node.
    pub(crate) fn node_properties(
        &self,
        id: NodeId,
    ) -> Result<Option<crate::graph::Properties>, Error> {
        let (Ok(mut labels), Ok(mut columns)) =
            (self.labels.try_borrow_mut(), self.columns.try_borrow_mut())
        else {
            return Ok(self.tx.node_properties(
                &mut LabelReader::default(),
                &mut ColumnReader::default(),
                id,
            )?);
        };
        Ok(self.tx.node_properties(&mut labels, &mut columns, id)?)
    }

    pub(crate) fn relationship(&self, id: RelationshipId) -> Result<RelationshipRecord, Error> {
        self.tx.relationship(id)?.ok_or_else(deleted)
    }

    /// The type of a relationship, which stays readable after the query deleted it.
    pub(crate) fn relationship_type(&self, id: RelationshipId) -> Result<String, Error> {
        let rel_type = match self.graveyard.relationships.get(&id) {
            Some(record) => record.rel_type,
            None => self.relationship(id)?.rel_type,
        };
        Ok(self.tx.token_name(rel_type)?)
    }

    pub(crate) fn label_names(&self, record: &NodeRecord) -> Result<Vec<String>, Error> {
        record
            .labels
            .iter()
            .map(|label| Ok(self.tx.token_name(*label)?))
            .collect()
    }

    pub(crate) fn property_map(
        &self,
        properties: &crate::graph::Properties,
    ) -> Result<BTreeMap<Arc<str>, Datum>, Error> {
        properties
            .iter()
            .map(|(key, value)| {
                Ok((
                    Arc::from(self.tx.token_name(*key)?),
                    Datum::from(value.clone()),
                ))
            })
            .collect()
    }

    #[inline]
    pub(crate) fn property_key(&self, key: &str) -> Result<Option<Token<PropertyKey>>, Error> {
        if let Some((_, token)) = self.keys.borrow().iter().find(|(name, _)| name == key) {
            return Ok(Some(*token));
        }
        let token = self.tx.lookup_token::<PropertyKey>(key)?;
        if let Some(token) = token {
            self.keys.borrow_mut().push((key.to_owned(), token));
        }
        Ok(token)
    }

    fn element_property(&self, value: &Datum, key: &str) -> Result<Option<Datum>, Error> {
        let token = self.property_key(key)?;
        let stored = match (value, token) {
            (Datum::Node(id), token) => self.node_property(*id, token)?,
            (Datum::Relationship(id), Some(token)) => self.tx.relationship_property(*id, token)?,
            (Datum::Relationship(id), None) => self.tx.relationship(*id)?.map(|_| None),
            _ => return Ok(None),
        };
        Ok(Some(
            stored.ok_or_else(deleted)?.map_or(Datum::Null, Datum::from),
        ))
    }

    pub(crate) fn property(&self, value: &Datum, key: &str) -> Result<Datum, Error> {
        match value {
            Datum::Null => Ok(Datum::Null),
            Datum::Map(entries) => Ok(entries.get(key).cloned().unwrap_or(Datum::Null)),
            Datum::Node(_) | Datum::Relationship(_) => {
                Ok(self.element_property(value, key)?.unwrap_or(Datum::Null))
            }
            Datum::Temporal(value) => temporals::component(value, key),
            other => Err(Error::new(
                ErrorKind::PropertyAccessOnNonEntity,
                format!("cannot read property `{key}` of a {}", other.type_name()),
            )),
        }
    }

    pub(crate) fn node_has_labels(
        &self,
        found: &[Token<Label>],
        labels: &LabelExpr,
    ) -> Result<bool, Error> {
        labels.matches(!found.is_empty(), &mut |name| {
            Ok(self
                .label_token(name)?
                .is_some_and(|token| found.binary_search(&token).is_ok()))
        })
    }

    pub(crate) fn predicate(&self, expr: &Expr, row: &Row) -> Result<bool, Error> {
        Ok(truth(&self.eval(expr, row)?)? == Some(true))
    }

    pub(crate) fn eval(&self, expr: &Expr, row: &Row) -> Result<Datum, Error> {
        Ok(self.tracked(expr, row)?.0)
    }

    fn tracked(&self, expr: &Expr, row: &Row) -> Result<(Datum, Taint), Error> {
        match self.eval_node(expr, row) {
            Err(error) if self.check.is_some() && !error.is_type_error() => {
                Ok((Datum::Null, Taint::Whole))
            }
            result => result,
        }
    }

    fn condition(&self, expr: &Expr, row: &Row) -> Result<(Option<bool>, bool), Error> {
        let (value, taint) = self.tracked(expr, row)?;
        match truth(&value) {
            Ok(truth) => Ok((truth, taint.is_tainted())),
            Err(_) if taint.is_whole() => Ok((None, true)),
            Err(error) => Err(error),
        }
    }

    fn bound<R>(&self, slot: Slot, taint: Taint, body: impl FnOnce() -> R) -> R {
        let Some(check) = self.check else {
            return body();
        };
        let mut bound = check.bound.take();
        bound.push((slot, taint));
        check.bound.set(bound);
        let result = body();
        let mut bound = check.bound.take();
        bound.pop();
        check.bound.set(bound);
        result
    }

    fn tracked_all(&self, exprs: &[Expr], row: &Row) -> Result<(Vec<Datum>, Vec<Taint>), Error> {
        let mut values = Vec::with_capacity(exprs.len());
        let mut taints = Vec::new();
        for expr in exprs {
            let (value, taint) = self.tracked(expr, row)?;
            values.push(value);
            if self.check.is_some() {
                taints.push(taint);
            }
        }
        Ok((values, taints))
    }

    fn slot_taint(&self, slot: Slot) -> Taint {
        let Some(check) = self.check else {
            return Taint::Clean;
        };
        let bound = check.bound.take();
        let taint = bound
            .iter()
            .rev()
            .find(|(bound, _)| *bound == slot)
            .map(|(_, taint)| taint.clone())
            .unwrap_or_default();
        check.bound.set(bound);
        taint
    }

    fn eval_node(&self, expr: &Expr, row: &Row) -> Result<(Datum, Taint), Error> {
        match expr {
            Expr::Constant(value) => Ok((value.clone(), Taint::Clean)),
            Expr::Parameter(index) => Ok((
                self.parameters.get(*index).cloned().unwrap_or(Datum::Null),
                Taint::Clean,
            )),
            Expr::Slot(slot) => Ok((
                row.get(*slot).cloned().unwrap_or(Datum::Null),
                self.slot_taint(*slot),
            )),
            Expr::List(items) => self.list(items, row),
            Expr::Map(entries) => self.map(entries, row),
            Expr::Property(base, key) => self.property_of(base, key, row),
            Expr::Index(base, index) => self.indexed(base, index, row),
            Expr::Slice(base, from, to) => self.sliced_value(base, from, to, row),
            Expr::HasLabels(operand, labels) => self.labels_test(operand, labels, row),
            Expr::Not(operand) => self.not(operand, row),
            Expr::Negate(operand) => self.negated(operand, row),
            Expr::UnaryPlus(operand) => self.plus(operand, row),
            Expr::Binary(op, lhs, rhs) => self.binary(*op, lhs, rhs, row),
            Expr::Comparison(first, rest) => self.comparison(first, rest, row),
            Expr::IsNull { operand, negated } => self.is_null(operand, *negated, row),
            Expr::Call(function, args) => self.called(*function, args, row),
            Expr::Admit(function, argument) => self.admitted(*function, argument, row),
            Expr::Case(case) => self.case(case, row),
            Expr::ListComprehension(comprehension) => self.comprehension(comprehension, row),
            Expr::Quantified(quantified) => self.quantified(quantified, row),
            Expr::Exists(query) => self.exists(query, row),
            Expr::CountMatches(query) => self.count_matches(query, row),
            Expr::Subquery(kind, clauses) => self.subquery(*kind, clauses, row),
            Expr::Reduce(reduce) => self.reduce(reduce, row),
            Expr::PatternComprehension(query, projection) => {
                self.pattern_comprehension(query, projection, row)
            }
            Expr::Typed(operand, types) => self.typed(operand, types, row),
            Expr::Cast(operand, target) => self.cast_value(operand, target, row),
            Expr::Truth(operand, truth) => self.truth(operand, *truth, row),
            Expr::Normalized(operand, form) => self.normalized(operand, *form, row),
            Expr::Distinct(a, b) => self.distinct(a, b, row),
        }
    }

    fn typed(&self, operand: &Expr, types: &TypeExpr, row: &Row) -> Result<(Datum, Taint), Error> {
        let (value, taint) = self.tracked(operand, row)?;
        Ok((
            Datum::Boolean(types::is_typed(&value, types)),
            Taint::when(taint.is_tainted()),
        ))
    }

    fn cast_value(
        &self,
        operand: &Expr,
        target: &ValueType,
        row: &Row,
    ) -> Result<(Datum, Taint), Error> {
        let (value, taint) = self.tracked(operand, row)?;
        let doubtful = taint.is_tainted();
        settle(self.cast(value, target), doubtful, Taint::when(doubtful))
    }

    fn distinct(&self, a: &Expr, b: &Expr, row: &Row) -> Result<(Datum, Taint), Error> {
        let (a, a_taint) = self.tracked(a, row)?;
        let (b, b_taint) = self.tracked(b, row)?;
        Ok((
            Datum::Boolean(Key(vec![a]) != Key(vec![b])),
            Taint::when(a_taint.is_tainted() || b_taint.is_tainted()),
        ))
    }

    fn truth(
        &self,
        operand: &Expr,
        truth: Option<bool>,
        row: &Row,
    ) -> Result<(Datum, Taint), Error> {
        let (value, taint) = self.tracked(operand, row)?;
        let doubtful = taint.is_whole();
        let result = match value {
            Datum::Null => Ok(Datum::Boolean(truth.is_none())),
            Datum::Boolean(value) => Ok(Datum::Boolean(truth == Some(value))),
            other => Err(type_error(format!(
                "a truth value test takes a boolean, not a {}",
                other.type_name()
            ))),
        };
        settle(result, doubtful, Taint::when(doubtful))
    }

    fn normalized(
        &self,
        operand: &Expr,
        form: NormalForm,
        row: &Row,
    ) -> Result<(Datum, Taint), Error> {
        let (value, taint) = self.tracked(operand, row)?;
        let doubtful = taint.is_whole();
        let result = match value {
            Datum::Null => Ok(Datum::Null),
            Datum::String(text) => Ok(Datum::Boolean(functions::normalized(&text, form) == *text)),
            other => Err(type_error(format!(
                "IS NORMALIZED takes a string, not a {}",
                other.type_name()
            ))),
        };
        settle(result, doubtful, Taint::when(doubtful))
    }

    fn list(&self, items: &[Expr], row: &Row) -> Result<(Datum, Taint), Error> {
        let (values, taints) = self.tracked_all(items, row)?;
        Ok((datum::list(values)?, Taint::list(taints)))
    }

    fn map(&self, entries: &[(String, Expr)], row: &Row) -> Result<(Datum, Taint), Error> {
        let mut values = BTreeMap::new();
        let mut taints = BTreeMap::new();
        for (key, value) in entries {
            let (value, taint) = self.tracked(value, row)?;
            if taint.is_tainted() {
                taints.insert(key.clone(), taint);
            } else {
                taints.remove(key);
            }
            values.insert(Arc::from(key.as_str()), value);
        }
        Ok((datum::map(values)?, Taint::map(taints)))
    }

    fn property_of(&self, base: &Expr, key: &str, row: &Row) -> Result<(Datum, Taint), Error> {
        if let Expr::Slot(slot) = base {
            let taint = self.slot_taint(*slot);
            return settle(
                self.property(row.get(*slot).unwrap_or(&Datum::Null), key),
                taint.is_whole(),
                taint.entry(key),
            );
        }
        let (base, taint) = self.tracked(base, row)?;
        settle(
            self.property(&base, key),
            taint.is_whole(),
            taint.entry(key),
        )
    }

    fn indexed(&self, base: &Expr, index: &Expr, row: &Row) -> Result<(Datum, Taint), Error> {
        let (base, base_taint) = self.tracked(base, row)?;
        let (index, index_taint) = self.tracked(index, row)?;
        let taint = if index_taint.is_tainted() {
            Taint::Whole
        } else {
            base_taint.at(&base, &index)
        };
        settle(
            self.index(base, index),
            base_taint.is_whole() || index_taint.is_whole(),
            taint,
        )
    }

    fn sliced_value(
        &self,
        base: &Expr,
        from: &Option<Box<Expr>>,
        to: &Option<Box<Expr>>,
        row: &Row,
    ) -> Result<(Datum, Taint), Error> {
        let (base, base_taint) = self.tracked(base, row)?;
        let (from, from_taint) = self.slice_bound(from.as_deref(), row)?;
        let (to, to_taint) = self.slice_bound(to.as_deref(), row)?;
        let doubtful = base_taint.is_whole() || from_taint.is_whole() || to_taint.is_whole();
        let taint = match base_taint {
            _ if from_taint.is_tainted() || to_taint.is_tainted() => Taint::Whole,
            Taint::List(items) => Taint::list(
                sliced(items, from.as_ref(), to.as_ref())
                    .ok()
                    .flatten()
                    .unwrap_or_default(),
            ),
            other => other.flatten(),
        };
        settle(slice(base, from.as_ref(), to.as_ref()), doubtful, taint)
    }

    fn labels_test(
        &self,
        operand: &Expr,
        labels: &LabelExpr,
        row: &Row,
    ) -> Result<(Datum, Taint), Error> {
        let (operand, taint) = self.tracked(operand, row)?;
        settle(
            self.has_labels(operand, labels),
            taint.is_whole(),
            taint.flatten(),
        )
    }

    fn not(&self, operand: &Expr, row: &Row) -> Result<(Datum, Taint), Error> {
        let (operand, taint) = self.tracked(operand, row)?;
        settle(
            truth(&operand).map(|value| from_truth(value.map(|value| !value))),
            taint.is_whole(),
            taint.flatten(),
        )
    }

    fn negated(&self, operand: &Expr, row: &Row) -> Result<(Datum, Taint), Error> {
        let (operand, taint) = self.tracked(operand, row)?;
        settle(negate(operand), taint.is_whole(), taint.flatten())
    }

    fn plus(&self, operand: &Expr, row: &Row) -> Result<(Datum, Taint), Error> {
        let (operand, taint) = self.tracked(operand, row)?;
        settle(unary_plus(operand), taint.is_whole(), taint.flatten())
    }

    fn binary(
        &self,
        op: BinaryOp,
        lhs: &Expr,
        rhs: &Expr,
        row: &Row,
    ) -> Result<(Datum, Taint), Error> {
        let (lhs, lhs_taint) = self.tracked(lhs, row)?;
        let (rhs, rhs_taint) = self.tracked(rhs, row)?;
        let result = match (op, &lhs, &rhs) {
            (BinaryOp::RegexMatch, Datum::String(text), Datum::String(pattern)) => {
                self.patterns.matches(text, pattern).map(Datum::Boolean)
            }
            _ => binary(op, lhs, rhs),
        };
        settle(
            result,
            lhs_taint.is_whole() || rhs_taint.is_whole(),
            Taint::when(lhs_taint.is_tainted() || rhs_taint.is_tainted()),
        )
    }

    fn comparison(
        &self,
        first: &Expr,
        rest: &[(ComparisonOp, Expr)],
        row: &Row,
    ) -> Result<(Datum, Taint), Error> {
        let (mut left, first_taint) = self.tracked(first, row)?;
        let mut tainted = first_taint.is_tainted();
        let mut result = Some(true);
        for (op, operand) in rest {
            let (right, taint) = self.tracked(operand, row)?;
            tainted |= taint.is_tainted();
            result = and(result, comparison(*op, &left, &right));
            left = right;
        }
        Ok((from_truth(result), Taint::when(tainted)))
    }

    fn is_null(&self, operand: &Expr, negated: bool, row: &Row) -> Result<(Datum, Taint), Error> {
        let (operand, taint) = self.tracked(operand, row)?;
        Ok((
            Datum::Boolean(operand.is_null() != negated),
            Taint::when(taint.is_whole()),
        ))
    }

    fn called(
        &self,
        function: Function,
        args: &[Expr],
        row: &Row,
    ) -> Result<(Datum, Taint), Error> {
        let (args, taints) = self.tracked_all(args, row)?;
        let (taint, doubtful) = taint::call(function, &args, taints);
        settle(self.call(function, args), doubtful, taint)
    }

    fn admitted(
        &self,
        function: AggregateFunction,
        argument: &Expr,
        row: &Row,
    ) -> Result<(Datum, Taint), Error> {
        let (value, taint) = self.tracked(argument, row)?;
        let doubtful = taint.is_tainted();
        settle(
            function.admit(&value).map(|()| value),
            doubtful,
            Taint::Whole,
        )
    }

    fn case(&self, case: &Case, row: &Row) -> Result<(Datum, Taint), Error> {
        let scrutinee = case
            .scrutinee
            .as_ref()
            .map(|scrutinee| self.tracked(scrutinee, row))
            .transpose()?;
        for (when, then) in &case.branches {
            let (hit, doubtful) = match &scrutinee {
                Some((scrutinee, scrutinee_taint)) => {
                    let (when, when_taint) = self.tracked(when, row)?;
                    (
                        equals(scrutinee, &when) == Some(true),
                        scrutinee_taint.is_tainted() || when_taint.is_tainted(),
                    )
                }
                None => {
                    let (truth, doubtful) = self.condition(when, row)?;
                    (truth == Some(true), doubtful)
                }
            };
            if doubtful {
                return Ok((Datum::Null, Taint::Whole));
            }
            if hit {
                return self.tracked(then, row);
            }
        }
        match &case.otherwise {
            Some(otherwise) => self.tracked(otherwise, row),
            None => Ok((Datum::Null, Taint::Clean)),
        }
    }

    fn comprehension(
        &self,
        comprehension: &ListComprehension,
        row: &Row,
    ) -> Result<(Datum, Taint), Error> {
        let (source, source_taint) = self.tracked(&comprehension.source, row)?;
        if source_taint.is_whole() {
            return Ok((Datum::Null, Taint::Whole));
        }
        let Some(items) = list_operand(source)? else {
            return Ok((Datum::Null, Taint::Clean));
        };
        let taints = source_taint.items();
        let mut local = row.clone();
        let mut values = Vec::new();
        let mut value_taints = Vec::new();
        let mut doubtful = false;
        for (item, taint) in items.into_iter().zip(taints) {
            set(&mut local, comprehension.slot, item.clone());
            let kept = self.bound(comprehension.slot, taint, || {
                if let Some(filter) = &comprehension.filter {
                    let (keep, unsure) = self.condition(filter, &local)?;
                    doubtful |= unsure;
                    if unsure || keep != Some(true) {
                        return Ok(None);
                    }
                }
                match &comprehension.projection {
                    Some(projection) => self.tracked(projection, &local).map(Some),
                    None => Ok(Some((item, self.slot_taint(comprehension.slot)))),
                }
            })?;
            if let Some((value, taint)) = kept {
                values.push(value);
                if self.check.is_some() {
                    value_taints.push(taint);
                }
            }
        }
        let taint = if doubtful {
            Taint::Whole
        } else {
            Taint::list(value_taints)
        };
        Ok((datum::list(values)?, taint))
    }

    fn quantified(&self, quantified: &Quantified, row: &Row) -> Result<(Datum, Taint), Error> {
        let (source, source_taint) = self.tracked(&quantified.source, row)?;
        if source_taint.is_whole() {
            return Ok((Datum::Null, Taint::Whole));
        }
        let Some(items) = list_operand(source)? else {
            return Ok((Datum::Null, Taint::Clean));
        };
        let taints = source_taint.items();
        let mut local = row.clone();
        let mut trues = 0usize;
        let mut falses = 0usize;
        let mut nulls = 0usize;
        let mut doubtful = false;
        for (item, taint) in items.into_iter().zip(taints) {
            set(&mut local, quantified.slot, item);
            let (truth, unsure) = self.bound(quantified.slot, taint, || {
                self.condition(&quantified.predicate, &local)
            })?;
            doubtful |= unsure;
            match truth {
                Some(true) => trues += 1,
                Some(false) => falses += 1,
                None => nulls += 1,
            }
        }
        let unknown = nulls > 0;
        let result = from_truth(match quantified.quantifier {
            Quantifier::All if falses > 0 => Some(false),
            Quantifier::All => (!unknown).then_some(true),
            Quantifier::Any if trues > 0 => Some(true),
            Quantifier::Any => (!unknown).then_some(false),
            Quantifier::None if trues > 0 => Some(false),
            Quantifier::None => (!unknown).then_some(true),
            Quantifier::Single if trues > 1 => Some(false),
            Quantifier::Single if unknown => None,
            Quantifier::Single => Some(trues == 1),
        });
        Ok((result, Taint::when(doubtful)))
    }

    fn exists(&self, query: &PatternQuery, row: &Row) -> Result<(Datum, Taint), Error> {
        let mut found = false;
        self.match_query(query, row, &mut |_| {
            found = true;
            Ok(false)
        })?;
        Ok((Datum::Boolean(found), Taint::Clean))
    }

    fn count_matches(&self, query: &PatternQuery, row: &Row) -> Result<(Datum, Taint), Error> {
        let mut found = 0i64;
        self.match_query(query, row, &mut |_| {
            found += 1;
            Ok(true)
        })?;
        Ok((Datum::Integer(found), Taint::Clean))
    }

    fn subquery(
        &self,
        kind: SubqueryKind,
        clauses: &[ReadClause],
        row: &Row,
    ) -> Result<(Datum, Taint), Error> {
        let mut rows = vec![row.clone()];
        for clause in clauses {
            rows = self.read_clause(clause, rows)?;
        }
        let value = match kind {
            SubqueryKind::Exists => Datum::Boolean(!rows.is_empty()),
            SubqueryKind::Count => Datum::Integer(i64::try_from(rows.len()).unwrap_or(i64::MAX)),
            SubqueryKind::Collect(slot) => datum::list(
                rows.into_iter()
                    .map(|mut row| {
                        row.get_mut(slot)
                            .map_or(Datum::Null, |value| std::mem::replace(value, Datum::Null))
                    })
                    .collect(),
            )?,
            SubqueryKind::Value(slot) => match rows.as_mut_slice() {
                [] => Datum::Null,
                [row] => row
                    .get_mut(slot)
                    .map_or(Datum::Null, |value| std::mem::replace(value, Datum::Null)),
                _ => {
                    return Err(Error::new(
                        ErrorKind::CardinalityViolation,
                        format!("a VALUE subquery returned {} rows", rows.len()),
                    ));
                }
            },
        };
        Ok((value, Taint::Clean))
    }

    fn reduce(&self, reduce: &Reduce, row: &Row) -> Result<(Datum, Taint), Error> {
        let (mut accumulator, mut accumulator_taint) = self.tracked(&reduce.init, row)?;
        let (source, source_taint) = self.tracked(&reduce.source, row)?;
        if source_taint.is_whole() {
            return Ok((Datum::Null, Taint::Whole));
        }
        let Some(items) = list_operand(source)? else {
            return Ok((Datum::Null, Taint::Clean));
        };
        let mut local = row.clone();
        for (item, taint) in items.into_iter().zip(source_taint.items()) {
            set(&mut local, reduce.accumulator, accumulator);
            set(&mut local, reduce.slot, item);
            (accumulator, accumulator_taint) =
                self.bound(reduce.accumulator, accumulator_taint, || {
                    self.bound(reduce.slot, taint, || self.tracked(&reduce.body, &local))
                })?;
        }
        Ok((accumulator, accumulator_taint))
    }

    fn pattern_comprehension(
        &self,
        query: &PatternQuery,
        projection: &Expr,
        row: &Row,
    ) -> Result<(Datum, Taint), Error> {
        let mut out = Vec::new();
        self.match_query(query, row, &mut |matched| {
            out.push(self.eval(projection, matched)?);
            Ok(true)
        })?;
        Ok((datum::list(out)?, Taint::Clean))
    }

    fn slice_bound(
        &self,
        bound: Option<&Expr>,
        row: &Row,
    ) -> Result<(Option<Datum>, Taint), Error> {
        match bound {
            None => Ok((None, Taint::Clean)),
            Some(bound) => {
                let (value, taint) = self.tracked(bound, row)?;
                Ok((Some(value), taint))
            }
        }
    }

    fn has_labels(&self, operand: Datum, labels: &LabelExpr) -> Result<Datum, Error> {
        match operand {
            Datum::Null => Ok(Datum::Null),
            Datum::Node(id) => Ok(Datum::Boolean(
                self.with_labels(id, |found| self.node_has_labels(found, labels))?
                    .ok_or_else(deleted)??,
            )),
            Datum::Relationship(id) => {
                let rel_type = self.relationship_type(id)?;
                let Ok(found) =
                    labels.matches::<Infallible>(true, &mut |name| Ok(name == rel_type));
                Ok(Datum::Boolean(found))
            }
            other => Err(type_error(format!(
                "labels can only be tested on a node or a relationship, not a {}",
                other.type_name()
            ))),
        }
    }

    fn index(&self, base: Datum, index: Datum) -> Result<Datum, Error> {
        match (base, index) {
            (Datum::Null, _) | (_, Datum::Null) => Ok(Datum::Null),
            (Datum::List(items), Datum::Integer(index)) => Ok(list_position(items.len(), index)
                .and_then(|position| items.get(position).cloned())
                .unwrap_or(Datum::Null)),
            (Datum::List(_), other) => Err(Error::new(
                ErrorKind::ListElementAccessByNonInteger,
                format!(
                    "a list index must be an integer, not a {}",
                    other.type_name()
                ),
            )),
            (Datum::Map(entries), Datum::String(key)) => {
                Ok(entries.get(key.as_str()).cloned().unwrap_or(Datum::Null))
            }
            (base @ (Datum::Node(_) | Datum::Relationship(_)), Datum::String(key)) => {
                self.property(&base, &key)
            }
            (Datum::Map(_) | Datum::Node(_) | Datum::Relationship(_), other) => Err(Error::new(
                ErrorKind::MapElementAccessByNonString,
                format!("a map key must be a string, not a {}", other.type_name()),
            )),
            (other, _) => Err(type_error(format!("cannot index a {}", other.type_name()))),
        }
    }
}

pub(crate) fn set(row: &mut Row, slot: Slot, value: Datum) {
    if let Some(entry) = row.get_mut(slot) {
        *entry = value;
    }
}

fn list_operand(value: Datum) -> Result<Option<Vec<Datum>>, Error> {
    match value {
        Datum::Null => Ok(None),
        Datum::List(items) => Ok(Some(Arc::unwrap_or_clone(items))),
        other => Err(type_error(format!(
            "expected a list, got a {}",
            other.type_name()
        ))),
    }
}

pub(crate) fn list_position(length: usize, index: i64) -> Option<usize> {
    let length = i64::try_from(length).ok()?;
    let position = if index < 0 { length + index } else { index };
    if (0..length).contains(&position) {
        usize::try_from(position).ok()
    } else {
        None
    }
}

fn slice(base: Datum, from: Option<&Datum>, to: Option<&Datum>) -> Result<Datum, Error> {
    let items = match base {
        Datum::Null => return Ok(Datum::Null),
        Datum::List(items) => Arc::unwrap_or_clone(items),
        other => return Err(type_error(format!("cannot slice a {}", other.type_name()))),
    };
    Ok(sliced(items, from, to)?.map_or(Datum::Null, |items| Datum::List(Arc::new(items))))
}

fn sliced<T>(
    items: Vec<T>,
    from: Option<&Datum>,
    to: Option<&Datum>,
) -> Result<Option<Vec<T>>, Error> {
    let length = i64::try_from(items.len()).unwrap_or(i64::MAX);
    let bound = |bound: Option<&Datum>, default: i64| -> Result<Option<i64>, Error> {
        match bound {
            None => Ok(Some(default)),
            Some(Datum::Null) => Ok(None),
            Some(Datum::Integer(value)) => {
                let value = if *value < 0 { length + value } else { *value };
                Ok(Some(value.clamp(0, length)))
            }
            Some(other) => Err(type_error(format!(
                "a slice bound must be an integer, not a {}",
                other.type_name()
            ))),
        }
    };
    let (Some(from), Some(to)) = (bound(from, 0)?, bound(to, length)?) else {
        return Ok(None);
    };
    if from >= to {
        return Ok(Some(Vec::new()));
    }
    let from = usize::try_from(from).unwrap_or(0);
    let to = usize::try_from(to).unwrap_or(0);
    Ok(Some(items.into_iter().skip(from).take(to - from).collect()))
}

fn negate(operand: Datum) -> Result<Datum, Error> {
    match operand {
        Datum::Null => Ok(Datum::Null),
        Datum::Integer(value) => value.checked_neg().map(Datum::Integer).ok_or_else(overflow),
        Datum::Float(value) => Ok(Datum::Float(-value)),
        other => Err(type_error(format!("cannot negate a {}", other.type_name()))),
    }
}

fn unary_plus(operand: Datum) -> Result<Datum, Error> {
    match operand {
        value @ (Datum::Null | Datum::Integer(_) | Datum::Float(_)) => Ok(value),
        other => Err(type_error(format!(
            "unary plus needs a number, not a {}",
            other.type_name()
        ))),
    }
}

fn settle(
    result: Result<Datum, Error>,
    doubtful: bool,
    taint: Taint,
) -> Result<(Datum, Taint), Error> {
    match result {
        Ok(value) => Ok((value, taint)),
        Err(_) if doubtful => Ok((Datum::Null, Taint::Whole)),
        Err(error) => Err(error),
    }
}

pub(crate) fn and(a: Option<bool>, b: Option<bool>) -> Option<bool> {
    match (a, b) {
        (Some(false), _) | (_, Some(false)) => Some(false),
        (Some(true), Some(true)) => Some(true),
        _ => None,
    }
}

pub(crate) fn or(a: Option<bool>, b: Option<bool>) -> Option<bool> {
    match (a, b) {
        (Some(true), _) | (_, Some(true)) => Some(true),
        (Some(false), Some(false)) => Some(false),
        _ => None,
    }
}

pub(crate) fn comparison(op: ComparisonOp, left: &Datum, right: &Datum) -> Option<bool> {
    match op {
        ComparisonOp::Equal => equals(left, right),
        ComparisonOp::NotEqual => equals(left, right).map(|equal| !equal),
        _ => match compare(left, right)? {
            Comparison::Unordered => Some(false),
            Comparison::Ordered(ordering) => Some(match op {
                ComparisonOp::Less => ordering.is_lt(),
                ComparisonOp::LessEqual => ordering.is_le(),
                ComparisonOp::Greater => ordering.is_gt(),
                _ => ordering.is_ge(),
            }),
        },
    }
}

fn binary(op: BinaryOp, lhs: Datum, rhs: Datum) -> Result<Datum, Error> {
    match op {
        BinaryOp::And => Ok(from_truth(and(truth(&lhs)?, truth(&rhs)?))),
        BinaryOp::Or => Ok(from_truth(or(truth(&lhs)?, truth(&rhs)?))),
        BinaryOp::Xor => {
            let (lhs, rhs) = (truth(&lhs)?, truth(&rhs)?);
            Ok(from_truth(lhs.zip(rhs).map(|(a, b)| a != b)))
        }
        BinaryOp::In => membership(&lhs, rhs),
        BinaryOp::RegexMatch => Ok(Datum::Null),
        BinaryOp::StartsWith | BinaryOp::EndsWith | BinaryOp::Contains => Ok(match (&lhs, &rhs) {
            (Datum::String(text), Datum::String(part)) => Datum::Boolean(match op {
                BinaryOp::StartsWith => text.starts_with(part.as_str()),
                BinaryOp::EndsWith => text.ends_with(part.as_str()),
                _ => text.contains(part.as_str()),
            }),
            _ => Datum::Null,
        }),
        BinaryOp::Add => add(lhs, rhs),
        BinaryOp::Concat => match (lhs, rhs) {
            (Datum::Null, _) | (_, Datum::Null) => Ok(Datum::Null),
            (Datum::String(a), Datum::String(b)) => Ok(Datum::String(a + &b)),
            (Datum::List(a), Datum::List(b)) => {
                let mut items = Arc::unwrap_or_clone(a);
                items.extend(Arc::unwrap_or_clone(b));
                datum::list(items)
            }
            (lhs, rhs) => Err(type_error(format!(
                "|| cannot join a {} and a {}",
                lhs.type_name(),
                rhs.type_name()
            ))),
        },
        BinaryOp::Subtract
        | BinaryOp::Multiply
        | BinaryOp::Divide
        | BinaryOp::Modulo
        | BinaryOp::Power => arithmetic(op, lhs, rhs),
    }
}

fn membership(needle: &Datum, haystack: Datum) -> Result<Datum, Error> {
    let items = match haystack {
        Datum::Null => return Ok(Datum::Null),
        Datum::List(items) => items,
        other => {
            return Err(type_error(format!(
                "IN needs a list on the right, not a {}",
                other.type_name()
            )));
        }
    };
    let mut unknown = false;
    for item in items.iter() {
        match equals(needle, item) {
            Some(true) => return Ok(Datum::Boolean(true)),
            None => unknown = true,
            Some(false) => {}
        }
    }
    Ok(if unknown {
        Datum::Null
    } else {
        Datum::Boolean(false)
    })
}

fn add(lhs: Datum, rhs: Datum) -> Result<Datum, Error> {
    match (lhs, rhs) {
        (Datum::Null, _) | (_, Datum::Null) => Ok(Datum::Null),
        (Datum::List(mut a), Datum::List(b)) => {
            Arc::make_mut(&mut a).extend(Arc::unwrap_or_clone(b));
            Ok(Datum::List(a))
        }
        (Datum::List(mut a), b) => {
            datum::check_item(&b)?;
            Arc::make_mut(&mut a).push(b);
            Ok(Datum::List(a))
        }
        (a, Datum::List(mut b)) => {
            datum::check_item(&a)?;
            Arc::make_mut(&mut b).insert(0, a);
            Ok(Datum::List(b))
        }
        (Datum::String(a), Datum::String(b)) => Ok(Datum::String(a + &b)),
        (Datum::String(a), b @ (Datum::Integer(_) | Datum::Float(_))) => {
            Ok(Datum::String(a + &number_text(&b)))
        }
        (a @ (Datum::Integer(_) | Datum::Float(_)), Datum::String(b)) => {
            Ok(Datum::String(number_text(&a) + &b))
        }
        (lhs, rhs) => arithmetic(BinaryOp::Add, lhs, rhs),
    }
}

/// Formats a number the way `toString` does.
pub(crate) fn number_text(value: &Datum) -> String {
    match value {
        Datum::Integer(value) => value.to_string(),
        Datum::Float(value) => float_text(*value),
        _ => String::new(),
    }
}

pub(crate) fn float_text(value: f64) -> String {
    if value.is_nan() {
        "NaN".to_owned()
    } else if value.is_infinite() {
        if value > 0.0 { "Infinity" } else { "-Infinity" }.to_owned()
    } else if value == 0.0 || (1e-3..1e7).contains(&value.abs()) {
        with_fraction(format!("{value}"))
    } else {
        let text = format!("{value:e}");
        match text.split_once('e') {
            Some((mantissa, exponent)) => format!("{}E{exponent}", with_fraction(mantissa.into())),
            None => text,
        }
    }
}

fn with_fraction(text: String) -> String {
    if text.contains('.') {
        text
    } else {
        text + ".0"
    }
}

pub(crate) fn arithmetic(op: BinaryOp, lhs: Datum, rhs: Datum) -> Result<Datum, Error> {
    if let Some(result) = temporals::arithmetic(op, &lhs, &rhs) {
        return result;
    }
    match (lhs, rhs) {
        (Datum::Null, _) | (_, Datum::Null) => Ok(Datum::Null),
        (Datum::Integer(a), Datum::Integer(b)) => integer_arithmetic(op, a, b),
        (Datum::Integer(a), Datum::Float(b)) => Ok(float_arithmetic(op, a as f64, b)),
        (Datum::Float(a), Datum::Integer(b)) => Ok(float_arithmetic(op, a, b as f64)),
        (Datum::Float(a), Datum::Float(b)) => Ok(float_arithmetic(op, a, b)),
        (a, b) => Err(type_error(format!(
            "cannot apply an arithmetic operator to a {} and a {}",
            a.type_name(),
            b.type_name()
        ))),
    }
}

fn integer_arithmetic(op: BinaryOp, a: i64, b: i64) -> Result<Datum, Error> {
    let divide_by_zero = || Error::new(ErrorKind::DivisionByZero, "integer division by zero");
    let result = match op {
        BinaryOp::Add => a.checked_add(b),
        BinaryOp::Subtract => a.checked_sub(b),
        BinaryOp::Multiply => a.checked_mul(b),
        BinaryOp::Divide => {
            if b == 0 {
                return Err(divide_by_zero());
            }
            a.checked_div(b)
        }
        BinaryOp::Modulo => {
            if b == 0 {
                return Err(divide_by_zero());
            }
            Some(a.wrapping_rem(b))
        }
        _ => return Ok(Datum::Float((a as f64).powf(b as f64))),
    };
    result.map(Datum::Integer).ok_or_else(overflow)
}

fn float_arithmetic(op: BinaryOp, a: f64, b: f64) -> Datum {
    Datum::Float(match op {
        BinaryOp::Add => a + b,
        BinaryOp::Subtract => a - b,
        BinaryOp::Multiply => a * b,
        BinaryOp::Divide => a / b,
        BinaryOp::Modulo => a % b,
        _ => a.powf(b),
    })
}
