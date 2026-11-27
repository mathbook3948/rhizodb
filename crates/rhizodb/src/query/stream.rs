use std::borrow::Cow;
use std::sync::Arc;

use crate::error::Error;
use crate::gql::ast::BinaryOp;
use crate::storage::ReadTransaction;

use super::aggregate::Groups;
use super::datum::Datum;
use super::eval::Env;
use super::eval::Row;
use super::eval::set;
use super::functions::Function;
use super::functions::range_items;
use super::keys::KeySet;
use super::plan::AggregateFunction;
use super::plan::CallClause;
use super::plan::CallSubquery;
use super::plan::Clause;
use super::plan::Expr;
use super::plan::Holds;
use super::plan::Items;
use super::plan::MatchClause;
use super::plan::NodePattern;
use super::plan::PatternPart;
use super::plan::Projection;
use super::plan::ReadClause;
use super::plan::Slot;
use super::plan::UpdateClause;
use super::seek;

/// How a streamed run ended: with every row it gave passed on, or with an error that applying
/// its clauses in turn might raise differently, so they must run that way instead.
pub(crate) enum Streamed {
    Done,
    Redo,
}

/// One clause of a streamed run, with what it keeps between rows.
pub(crate) struct Stage<'c> {
    clause: &'c ReadClause,
    state: State<'c>,
    /// Whether an error it raises is the one applying the clauses in turn raises.
    pub(crate) exact: bool,
}

enum State<'c> {
    /// Passes on each row it gives as it gives it.
    Pass,
    /// A match, which reads its start nodes as a column scan while `scans` holds.
    Match { scans: bool },
    /// A projection without `ORDER BY` or aggregates.
    Plain(Plain),
    /// A projection with `ORDER BY`, holding its rows until its input ends.
    Sorted(Vec<Row>),
    /// An aggregating projection, holding its groups until its input ends.
    Grouped(Groups<'c>),
}

struct Plain {
    skip: usize,
    /// `SKIP` plus `LIMIT`, the rows it reads before it gives no more.
    pub(crate) kept: Option<usize>,
    seen: Option<KeySet>,
    /// The rows counted toward `SKIP` and `LIMIT` so far.
    pub(crate) passed: usize,
    /// Whether no item can fail on a row, so a row past the limit need not be evaluated once
    /// one row was.
    pub(crate) sure: bool,
    evaluated: bool,
    /// Whether it may stop the clauses before it once it has its rows.
    pub(crate) stops: bool,
}

struct Stream<'s, 'e, T: ReadTransaction> {
    env: &'s Env<'e, T>,
    stages: Vec<Stage<'s>>,
    /// Whether an error the sink raises is the one applying the clauses in turn raises.
    pub(crate) exact: bool,
    /// The stage the first error came from.
    pub(crate) failed: Option<usize>,
    sink: &'s mut dyn FnMut(Row) -> Result<bool, Error>,
}

impl<T: ReadTransaction> Env<'_, T> {
    /// How many of `clauses`, the rest of a pipeline, a streamed run takes: up to the last
    /// projection that limits or aggregates its rows before a table call, which then never
    /// holds the rows the clauses before it give. `None` when no such projection comes first.
    pub(crate) fn streamed(&self, clauses: &[&ReadClause]) -> Option<usize> {
        if !self.streams() {
            return None;
        }
        let mut end = None;
        for (at, clause) in clauses.iter().enumerate() {
            match clause {
                ReadClause::CallSubquery(call) if call.table => break,
                ReadClause::Project(projection)
                    if projection.limit.is_some()
                        || matches!(projection.items, Items::Aggregate { .. }) =>
                {
                    end = Some(at + 1);
                }
                _ => {}
            }
        }
        end
    }

    /// Whether rows may stream through clauses, which they do unless evaluation only looks for
    /// type errors.
    pub(crate) fn streams(&self) -> bool {
        self.check.is_none() && !batch_only()
    }

    /// Whether the streamed run of `clauses` from one row, which [`Env::streamed`] gave as
    /// `streamed`, stops reading its first clause once a `LIMIT` has its rows.
    pub(crate) fn stops_early(&self, clauses: &[&ReadClause], streamed: Option<usize>) -> bool {
        let Some(count) = streamed else {
            return false;
        };
        let sure = Sure::new(self);
        let mut stops = false;
        sure.exactly(&clauses[..count], true, |clause, exact| {
            stops |= exact
                && matches!(clause, ReadClause::Project(projection)
                    if projection.order_by.is_empty() && projection.limit.is_some()
                        && sure.plain(projection));
            Some(())
        });
        stops
    }

    /// Applies `clauses`, which [`Env::streamed`] took, passing each row from one clause to
    /// the next as it comes.
    pub(crate) fn stream_clauses(
        &self,
        clauses: &[&ReadClause],
        rows: Vec<Row>,
    ) -> Result<Vec<Row>, Error> {
        let mut out = Vec::new();
        match self.each_row(clauses, &rows, &mut |row, _| {
            out.push(row);
            Ok(true)
        })? {
            Streamed::Done => Ok(out),
            Streamed::Redo => self.in_turn(clauses, rows),
        }
    }

    /// Applies `clauses` one at a time, each to every row.
    pub(crate) fn in_turn(
        &self,
        clauses: &[&ReadClause],
        mut rows: Vec<Row>,
    ) -> Result<Vec<Row>, Error> {
        for clause in clauses {
            rows = self.read_clause(clause, rows)?;
        }
        Ok(rows)
    }

    /// Passes the rows that `clauses` give from `rows` to `sink` in the order applying them in
    /// turn gives them, without collecting the rows between clauses. `sink` also learns whether
    /// it may stop the run, which holds when no row it would leave unread could fail, and stops
    /// it by returning false.
    pub(crate) fn each_row(
        &self,
        clauses: &[&ReadClause],
        rows: &[Row],
        sink: &mut dyn FnMut(Row, bool) -> Result<bool, Error>,
    ) -> Result<Streamed, Error> {
        let Some((stages, exact)) = self.stages(clauses, rows.len() == 1) else {
            return Ok(Streamed::Redo);
        };
        let mut sink = |row| sink(row, exact);
        let mut stream = Stream {
            env: self,
            stages,
            exact,
            failed: None,
            sink: &mut sink,
        };
        match stream.run(rows) {
            Ok(()) => Ok(Streamed::Done),
            Err(error) => match stream.failed {
                Some(at) if stream.exact(at) => Err(error),
                _ => Ok(Streamed::Redo),
            },
        }
    }

    /// The stages of a streamed run of `clauses` and whether an error the sink raises is exact.
    /// `None` when a stage cannot start, as when its `LIMIT` fails, so the clauses run in turn
    /// instead. Kept out of [`Env::each_row`], whose frame stays on the stack while a subquery
    /// nested in the run's expressions runs.
    #[inline(never)]
    pub(crate) fn stages<'c>(
        &self,
        clauses: &[&'c ReadClause],
        single: bool,
    ) -> Option<(Vec<Stage<'c>>, bool)> {
        let sure = Sure::new(self);
        let mut stages = Vec::with_capacity(clauses.len());
        let exact = sure.exactly(clauses, single, |clause, exact| {
            stages.push(self.stage(&sure, clause, exact).ok()?);
            Some(())
        })?;
        if stages
            .iter()
            .any(|stage| matches!(&stage.state, State::Plain(plain) if plain.stops))
        {
            self.stopping_at_limit();
        }
        Some((stages, exact))
    }

    fn stage<'c>(
        &self,
        sure: &Sure<'_, '_, T>,
        clause: &'c ReadClause,
        exact: bool,
    ) -> Result<Stage<'c>, Error> {
        let state = match clause {
            ReadClause::Project(projection) => match &projection.items {
                Items::Aggregate {
                    keys, aggregates, ..
                } => State::Grouped(Groups::new(keys, aggregates, self.limits)),
                Items::Plain(_) if !projection.order_by.is_empty() => State::Sorted(Vec::new()),
                Items::Plain(_) => {
                    let (skip, limit) = self.window(projection)?;
                    let kept = limit.map(|limit| skip.saturating_add(limit));
                    let sure = sure.plain(projection);
                    State::Plain(Plain {
                        skip,
                        kept,
                        seen: projection.distinct.then(KeySet::default),
                        passed: 0,
                        sure,
                        evaluated: false,
                        stops: exact && sure,
                    })
                }
            },
            ReadClause::Match(found) => State::Match {
                scans: self.node_scan(found)?.is_some(),
            },
            _ => State::Pass,
        };
        Ok(Stage {
            clause,
            state,
            exact,
        })
    }
}

/// Whether a clause holds every row it reads before it gives any.
pub(crate) fn breaks(clause: &ReadClause) -> bool {
    matches!(clause, ReadClause::Project(projection)
        if !projection.order_by.is_empty() || matches!(projection.items, Items::Aggregate { .. }))
}

impl<T: ReadTransaction> Stream<'_, '_, T> {
    fn run(&mut self, rows: &[Row]) -> Result<(), Error> {
        if self.scan_first(rows)? {
            return self.finish();
        }
        for row in rows {
            if !self.push(0, Cow::Borrowed(row))? {
                break;
            }
        }
        self.finish()
    }

    #[inline(never)]
    fn scan_first(&mut self, rows: &[Row]) -> Result<bool, Error> {
        if let Some(Stage {
            clause: ReadClause::Match(found),
            state: State::Match { scans: true },
            ..
        }) = self.stages.first()
        {
            let found: &MatchClause = found;
            let env = self.env;
            if let Some(scan) = env.node_scan(found).inspect_err(|_| self.fail(0))? {
                let scanned =
                    env.scan_rows(scan, found, rows, &mut |row| self.push(1, Cow::Owned(row)));
                match scanned {
                    Ok(Some(_)) => return Ok(true),
                    Ok(None) => self.stages[0].state = State::Match { scans: false },
                    Err(error) => {
                        self.fail(0);
                        return Err(error);
                    }
                }
            }
        }
        Ok(false)
    }

    fn fail(&mut self, at: usize) {
        self.failed.get_or_insert(at);
    }

    /// Whether an error stage `at`, or the sink past the last stage, raises is exact.
    pub(crate) fn exact(&self, at: usize) -> bool {
        self.stages.get(at).map_or(self.exact, |stage| stage.exact)
    }

    /// Passes the rows each holding stage held to the stages after it, in order.
    pub(crate) fn finish(&mut self) -> Result<(), Error> {
        for at in 0..self.stages.len() {
            let stage = &mut self.stages[at];
            let ReadClause::Project(projection) = stage.clause else {
                continue;
            };
            let rows = match &mut stage.state {
                State::Sorted(rows) => std::mem::take(rows),
                State::Grouped(groups) => {
                    let Items::Aggregate { finals, .. } = &projection.items else {
                        continue;
                    };
                    let groups = std::mem::replace(groups, Groups::new(&[], &[], self.env.limits));
                    groups
                        .finish(self.env, finals)
                        .inspect_err(|_| self.fail(at))?
                }
                State::Pass | State::Match { .. } | State::Plain(_) => continue,
            };
            let rows = self
                .env
                .arrange(projection, rows)
                .inspect_err(|_| self.fail(at))?;
            for row in rows {
                if !self.push(at + 1, Cow::Owned(row))? {
                    break;
                }
            }
        }
        Ok(())
    }

    /// Passes `row` to stage `at`, and says whether the stages from `at` on want more rows.
    pub(crate) fn push(&mut self, at: usize, row: Cow<'_, Row>) -> Result<bool, Error> {
        let result = self.step(at, row);
        if result.is_err() {
            self.fail(at);
        }
        result
    }

    fn step(&mut self, at: usize, row: Cow<'_, Row>) -> Result<bool, Error> {
        let Some(stage) = self.stages.get(at) else {
            return (self.sink)(row.into_owned());
        };
        match stage.clause {
            ReadClause::Match(found) => self.found(at, found, row),
            ReadClause::Unwind { expr, slot } => self.unwind(at, expr, *slot, row),
            ReadClause::Filter(condition) => self.filter(at, condition, row),
            ReadClause::Let(bindings) => self.bind(at, bindings, row),
            ReadClause::Call(call) => self.call(at, call, row),
            ReadClause::CallSubquery(call) => self.call_subquery(at, call, row),
            ReadClause::Project(projection) => self.project(at, projection, row),
        }
    }

    #[inline(never)]
    fn found(&mut self, at: usize, found: &MatchClause, row: Cow<'_, Row>) -> Result<bool, Error> {
        if matches!(self.stages[at].state, State::Match { scans: true })
            && let Some(more) = self.scanned(at, found, &row)?
        {
            return Ok(more);
        }
        let env = self.env;
        env.match_row(found, row.into_owned(), &mut |matched| {
            self.push(at + 1, Cow::Owned(matched.clone()))
        })
    }

    #[inline(never)]
    fn scanned(
        &mut self,
        at: usize,
        found: &MatchClause,
        row: &Row,
    ) -> Result<Option<bool>, Error> {
        let env = self.env;
        if let Some(scan) = env.node_scan(found)? {
            let mut emit = |row| self.push(at + 1, Cow::Owned(row));
            if let Some(more) = env.scan_rows(scan, found, std::slice::from_ref(row), &mut emit)? {
                return Ok(Some(more));
            }
        }
        self.stages[at].state = State::Match { scans: false };
        Ok(None)
    }

    #[inline(never)]
    fn unwind(
        &mut self,
        at: usize,
        expr: &Expr,
        slot: Slot,
        row: Cow<'_, Row>,
    ) -> Result<bool, Error> {
        let env = self.env;
        if let Expr::Call(Function::Range, args) = expr {
            let args = args
                .iter()
                .map(|arg| env.eval(arg, &row))
                .collect::<Result<Vec<_>, _>>()?;
            for item in range_items(args, env.limits)?.into_iter().flatten() {
                let mut next = row.clone().into_owned();
                set(&mut next, slot, Datum::Integer(item));
                if !self.push(at + 1, Cow::Owned(next))? {
                    return Ok(false);
                }
            }
            return Ok(true);
        }
        let items = match env.eval(expr, &row)? {
            Datum::Null => Arc::default(),
            Datum::List(items) => items,
            other => Arc::new(vec![other].into()),
        };
        for item in items.iter() {
            let mut next = row.clone().into_owned();
            set(&mut next, slot, item.clone());
            if !self.push(at + 1, Cow::Owned(next))? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    #[inline(never)]
    fn filter(&mut self, at: usize, condition: &Expr, row: Cow<'_, Row>) -> Result<bool, Error> {
        if self.env.predicate(condition, &row)? {
            return self.push(at + 1, row);
        }
        Ok(true)
    }

    #[inline(never)]
    fn bind(
        &mut self,
        at: usize,
        bindings: &[(Expr, Slot)],
        row: Cow<'_, Row>,
    ) -> Result<bool, Error> {
        let mut row = row.into_owned();
        for (expr, slot) in bindings {
            let value = self.env.eval(expr, &row)?;
            set(&mut row, *slot, value);
        }
        self.push(at + 1, Cow::Owned(row))
    }

    #[inline(never)]
    fn call(&mut self, at: usize, call: &CallClause, row: Cow<'_, Row>) -> Result<bool, Error> {
        let env = self.env;
        env.call_row(call, row.into_owned(), &mut |next| {
            self.push(at + 1, Cow::Owned(next))
        })
    }

    #[inline(never)]
    fn call_subquery(
        &mut self,
        at: usize,
        call: &CallSubquery<ReadClause>,
        row: Cow<'_, Row>,
    ) -> Result<bool, Error> {
        let mut out = Vec::new();
        self.env.subquery_row(call, row.into_owned(), &mut out)?;
        for next in out {
            if !self.push(at + 1, Cow::Owned(next))? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    #[inline(never)]
    fn project(
        &mut self,
        at: usize,
        projection: &Projection,
        row: Cow<'_, Row>,
    ) -> Result<bool, Error> {
        let env = self.env;
        let state = &mut self.stages[at].state;
        if let State::Grouped(groups) = state {
            groups.add(env, &row)?;
            return Ok(true);
        }
        let Items::Plain(items) = &projection.items else {
            return Ok(true);
        };
        let evaluate = |row: &mut Row| -> Result<(), Error> {
            for (expr, slot) in items {
                let value = env.eval(expr, row)?;
                set(row, *slot, value);
            }
            Ok(())
        };
        let plain = match state {
            State::Sorted(rows) => {
                let mut row = row.into_owned();
                evaluate(&mut row)?;
                rows.push(row);
                return Ok(true);
            }
            State::Plain(plain) => plain,
            State::Pass | State::Match { .. } | State::Grouped(_) => return Ok(true),
        };
        if plain.kept.is_some_and(|kept| plain.passed >= kept) {
            if !plain.sure || !plain.evaluated {
                plain.evaluated = true;
                evaluate(&mut row.into_owned())?;
            }
            return Ok(!plain.stops);
        }
        let mut row = row.into_owned();
        plain.evaluated = true;
        evaluate(&mut row)?;
        if let Some(seen) = &mut plain.seen {
            let outputs: Vec<Datum> = projection
                .outputs
                .iter()
                .map(|slot| row.get(*slot).cloned().unwrap_or(Datum::Null))
                .collect();
            if !seen.insert(&outputs) {
                return Ok(true);
            }
        }
        let index = plain.passed;
        plain.passed += 1;
        let full = plain.stops && plain.kept.is_some_and(|kept| plain.passed >= kept);
        if index < plain.skip {
            return Ok(!full);
        }
        if let Some(filter) = &projection.filter
            && !env.predicate(filter, &row)?
        {
            return Ok(!full);
        }
        let more = self.push(at + 1, Cow::Owned(row))?;
        Ok(more && !full)
    }
}

/// What the plan and the parameters prove about which expressions cannot fail.
pub(crate) struct Sure<'q, 'e, T: ReadTransaction> {
    env: &'q Env<'e, T>,
    /// Whether the query deleted a graph element, so reading one may fail.
    pub(crate) deleted: bool,
}

impl<'q, 'e, T: ReadTransaction> Sure<'q, 'e, T> {
    fn new(env: &'q Env<'e, T>) -> Self {
        let graveyard = env.graveyard;
        Self {
            env,
            deleted: !graveyard.deleted_nodes.is_empty()
                || !graveyard.nodes.is_empty()
                || !graveyard.relationships.is_empty(),
        }
    }

    /// Passes each of `clauses` to `each` with whether an error it raises is the first error
    /// applying the clauses in turn raises: every clause since the last one that holds its rows
    /// is settled, so none of them fails on a row it has not read yet. Gives the same for the
    /// rows the last clause passes on, or `None` once `each` does. `single` says the first
    /// clause reads a single row.
    pub(crate) fn exactly<'c>(
        &self,
        clauses: &[&'c ReadClause],
        single: bool,
        mut each: impl FnMut(&'c ReadClause, bool) -> Option<()>,
    ) -> Option<bool> {
        let mut clean = true;
        for (at, clause) in clauses.iter().enumerate() {
            each(clause, clean)?;
            clean = breaks(clause) || (clean && self.settled(clause, at == 0 && single));
        }
        Some(clean)
    }

    /// Whether the work of `clause` on the rows it has not read yet cannot fail, once one row
    /// has passed it. `only` says it reads a single row.
    pub(crate) fn settled(&self, clause: &ReadClause, only: bool) -> bool {
        match clause {
            ReadClause::Match(found) => self.matching(found),
            ReadClause::Call(call) if only => call
                .filter
                .as_ref()
                .is_none_or(|filter| self.condition(filter, true)),
            _ if only => true,
            ReadClause::Unwind { expr, .. } => self.value(expr, true),
            ReadClause::Filter(condition) => self.condition(condition, true),
            ReadClause::Let(bindings) => bindings.iter().all(|(expr, _)| self.value(expr, true)),
            ReadClause::Project(projection) => !breaks(clause) && self.plain(projection),
            ReadClause::Call(_) | ReadClause::CallSubquery(_) => false,
        }
    }

    /// Whether no item or filter of a projection without aggregates can fail on a row.
    pub(crate) fn plain(&self, projection: &Projection) -> bool {
        let Items::Plain(items) = &projection.items else {
            return false;
        };
        items.iter().all(|(expr, _)| self.value(expr, true))
            && projection
                .filter
                .as_ref()
                .is_none_or(|filter| self.condition(filter, true))
    }

    /// Whether the expression reads nothing that differs between rows, so it gives the same
    /// value or error on every row.
    pub(crate) fn fixed(&self, expr: &Expr) -> bool {
        seek::fixed(expr)
    }

    /// Whether evaluating `expr` cannot fail on any row. With `fixed_fails`, an expression that
    /// [`Sure::fixed`] holds for counts as unable to fail, which is right where it was
    /// evaluated at least once already without failing.
    pub(crate) fn value(&self, expr: &Expr, fixed_fails: bool) -> bool {
        if fixed_fails && self.fixed(expr) {
            return true;
        }
        match expr {
            Expr::Constant(_) | Expr::Parameter(_) | Expr::Slot(_) => true,
            Expr::Property(base, _) => self.element(base) || self.map_like(base),
            _ => self.condition(expr, fixed_fails),
        }
    }

    /// Whether `expr` gives a boolean or null on every row and cannot fail.
    pub(crate) fn condition(&self, expr: &Expr, fixed_fails: bool) -> bool {
        if fixed_fails && self.fixed(expr) {
            return true;
        }
        let value = |expr| self.value(expr, fixed_fails);
        let condition = |expr| self.condition(expr, fixed_fails);
        match expr {
            Expr::Constant(value) => matches!(value, Datum::Boolean(_) | Datum::Null),
            Expr::Parameter(index) => matches!(
                self.env.parameters.get(*index),
                Some(Datum::Boolean(_) | Datum::Null) | None
            ),
            Expr::Comparison(first, rest) => {
                value(first) && rest.iter().all(|(_, operand)| value(operand))
            }
            Expr::IsNull { operand, .. } => value(operand),
            Expr::Typed(operand, _) => value(operand),
            Expr::Distinct(a, b) => value(a) && value(b),
            Expr::Not(operand) => condition(operand),
            Expr::HasLabels(operand, _) => self.element(operand),
            Expr::Binary(BinaryOp::And | BinaryOp::Or | BinaryOp::Xor, lhs, rhs) => {
                condition(lhs) && condition(rhs)
            }
            Expr::Binary(
                BinaryOp::StartsWith | BinaryOp::EndsWith | BinaryOp::Contains,
                lhs,
                rhs,
            ) => value(lhs) && value(rhs),
            Expr::Binary(BinaryOp::In, lhs, rhs) => value(lhs) && self.list_like(rhs),
            Expr::Exists(query) => {
                self.patterns(&query.patterns, false)
                    && query
                        .filter
                        .as_ref()
                        .is_none_or(|filter| self.condition(filter, false))
            }
            _ => false,
        }
    }

    /// Whether `expr` is a variable that holds a live graph element or null.
    pub(crate) fn element(&self, expr: &Expr) -> bool {
        matches!(expr, Expr::Slot(slot) if !self.deleted && self.env.holds.get(*slot) == Some(&Holds::Element))
    }

    fn map_like(&self, expr: &Expr) -> bool {
        match expr {
            Expr::Constant(value) => matches!(value, Datum::Map(_) | Datum::Null),
            Expr::Parameter(index) => matches!(
                self.env.parameters.get(*index),
                Some(Datum::Map(_) | Datum::Null) | None
            ),
            _ => false,
        }
    }

    fn list_like(&self, expr: &Expr) -> bool {
        match expr {
            Expr::Constant(value) => matches!(value, Datum::List(_) | Datum::Null),
            Expr::Parameter(index) => matches!(
                self.env.parameters.get(*index),
                Some(Datum::List(_) | Datum::Null) | None
            ),
            _ => false,
        }
    }

    /// Whether matching `found` cannot fail on any row. An optional match may give a row
    /// without testing its patterns, so only a match that is not optional takes expressions
    /// that [`Sure::fixed`] holds for as unable to fail.
    pub(crate) fn matching(&self, found: &MatchClause) -> bool {
        let once = !found.optional;
        let clause = |found: &MatchClause| {
            self.patterns(&found.patterns, once)
                && found
                    .filter
                    .as_ref()
                    .is_none_or(|filter| self.condition(filter, once))
        };
        clause(found)
            && found
                .by_id
                .as_deref()
                .is_none_or(|by_id| clause(&by_id.clause))
    }

    fn patterns(&self, parts: &[PatternPart], once: bool) -> bool {
        parts.iter().all(|part| {
            self.node(&part.start, once)
                && part.steps.iter().all(|step| {
                    let single = step.relationship.length.is_none();
                    self.node(&step.node, once)
                        && step
                            .relationship
                            .properties
                            .as_ref()
                            .is_none_or(|properties| self.properties(properties, once && single))
                })
                && self.patterns(&part.others, once)
        })
    }

    fn node(&self, pattern: &NodePattern, once: bool) -> bool {
        pattern
            .properties
            .as_ref()
            .is_none_or(|properties| self.properties(properties, once))
    }

    fn properties(&self, properties: &Expr, once: bool) -> bool {
        (once && self.fixed(properties)) || self.map_like(properties)
    }
}

/// What the clauses of a plan bind slots to, which [`Bindings::holds`] turns into what each
/// slot holds.
#[derive(Default)]
pub(crate) struct Bindings {
    holds: Vec<(Slot, Holds)>,
    /// Pairs of a slot and the slot a projection or `LET` copies it to.
    pub(crate) copies: Vec<(Slot, Slot)>,
}

impl Bindings {
    pub(crate) fn read(&mut self, clause: &ReadClause) {
        match clause {
            ReadClause::Match(found) => {
                self.patterns(&found.patterns);
                if let Some(by_id) = &found.by_id {
                    self.patterns(&by_id.clause.patterns);
                }
            }
            ReadClause::Unwind { expr, slot } => {
                let integers = match expr {
                    Expr::Call(Function::Range, _) => true,
                    Expr::Constant(Datum::List(items)) => items
                        .iter()
                        .all(|item| matches!(item, Datum::Integer(_) | Datum::Null)),
                    _ => false,
                };
                if integers {
                    self.holds.push((*slot, Holds::Integer));
                }
            }
            ReadClause::Project(projection) => match &projection.items {
                Items::Plain(items) => self.copies(items),
                Items::Aggregate {
                    keys,
                    aggregates,
                    finals,
                } => {
                    self.copies(keys);
                    self.copies(finals);
                    for (aggregate, slot) in aggregates {
                        if aggregate.function == AggregateFunction::Count {
                            self.holds.push((*slot, Holds::Integer));
                        }
                    }
                }
            },
            ReadClause::Let(bindings) => self.copies(bindings),
            ReadClause::CallSubquery(call) => {
                for branch in &call.branches {
                    for clause in &branch.clauses {
                        self.read(clause);
                    }
                }
            }
            ReadClause::Call(_) | ReadClause::Filter(_) => {}
        }
    }

    pub(crate) fn update(&mut self, clause: &UpdateClause) {
        match clause {
            UpdateClause::Create(parts) => self.patterns(parts),
            UpdateClause::Merge(merge) => self.patterns(std::slice::from_ref(&merge.pattern)),
            UpdateClause::CallSubquery(call) => {
                for branch in &call.branches {
                    for clause in &branch.clauses {
                        match clause {
                            Clause::Read(clause) => self.read(clause),
                            Clause::Update(clause) => self.update(clause),
                        }
                    }
                }
            }
            UpdateClause::Set(_) | UpdateClause::Remove(_) | UpdateClause::Delete { .. } => {}
        }
    }

    fn patterns(&mut self, parts: &[PatternPart]) {
        for part in parts {
            if !part.start.bound {
                self.holds.push((part.start.slot, Holds::Element));
            }
            for step in &part.steps {
                if !step.node.bound {
                    self.holds.push((step.node.slot, Holds::Element));
                }
                if !step.relationship.bound && step.relationship.length.is_none() {
                    self.holds.push((step.relationship.slot, Holds::Element));
                }
            }
        }
    }

    fn copies(&mut self, pairs: &[(Expr, Slot)]) {
        for (expr, slot) in pairs {
            if let Expr::Slot(from) = expr {
                self.copies.push((*from, *slot));
            }
        }
    }

    /// Per slot of `slots`, what it holds. A copy is bound after the slot it copies, which
    /// therefore has the lower number, so following the copies in order of their sources
    /// reaches the end of every chain of copies.
    pub(crate) fn holds(mut self, slots: usize) -> Vec<Holds> {
        let mut holds = vec![Holds::Any; slots];
        for (slot, held) in self.holds {
            if let Some(entry) = holds.get_mut(slot) {
                *entry = held;
            }
        }
        self.copies.sort_unstable();
        for (from, to) in self.copies {
            if let Some(&held) = holds.get(from)
                && let Some(entry) = holds.get_mut(to)
            {
                *entry = held;
            }
        }
        holds
    }
}

#[cfg(not(test))]
fn batch_only() -> bool {
    false
}

#[cfg(test)]
use tests::batch_only;
#[cfg(test)]
pub(crate) use tests::produced;

#[cfg(test)]
#[path = "stream.test.rs"]
mod tests;
