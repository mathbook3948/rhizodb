use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::error::Error;
use crate::error::ErrorKind;
use crate::gql::ast::SortDirection;
use crate::gql::ast::UnionKind;
use crate::graph::GraphTx;
use crate::graph::NodeId;
use crate::procedure::Procedure;
use crate::procedure::ValueType;
use crate::result;
use crate::result::QueryResult;
use crate::result::Value;
use crate::storage::ReadTransaction;
use crate::storage::WriteTransaction;
use crate::temporal::Clocks;

use super::aggregate::Groups;
use super::compare::sort_order;
use super::datum::Datum;
use super::datum::PathIds;
use super::datum::dismantle;
use super::datum::from_parameter;
use super::eval::Check;
use super::eval::Env;
use super::eval::Row;
use super::eval::set;
use super::keys::KeyIndex;
use super::keys::KeySet;
use super::matcher::counted_nodes;
use super::plan::Aggregate;
use super::plan::CallClause;
use super::plan::CallSubquery;
use super::plan::Callee;
use super::plan::Clause;
use super::plan::Expr;
use super::plan::Items;
use super::plan::MatchClause;
use super::plan::Pipeline;
use super::plan::Program;
use super::plan::Projection;
use super::plan::ReadClause;
use super::plan::Slot;
use super::regex::Patterns;
use super::seek;
use super::shape::Shape;
use super::shape::admits;
use super::shape::procedure_parameter;
use super::update::Graveyard;
use super::update::Writer;

/// Runs a read-only program. It only needs a read transaction.
pub(crate) fn run_read<T: ReadTransaction>(
    tx: &GraphTx<T>,
    program: &Program<ReadClause>,
    parameters: &[Datum],
    now: &Clocks,
) -> Result<QueryResult, Error> {
    read_program(tx, program, parameters, now, |env, clause, next, rows| {
        env.read_clauses(clause, next, rows)
    })
}

/// Runs a read-only program whose clauses `read` applies, as [`Env::read_clauses`] does.
pub(crate) fn read_program<T: ReadTransaction>(
    tx: &GraphTx<T>,
    program: &Program<ReadClause>,
    parameters: &[Datum],
    now: &Clocks,
    read: impl Fn(
        &Env<'_, T>,
        &ReadClause,
        Option<&ReadClause>,
        Vec<Row>,
    ) -> Result<(Vec<Row>, usize), Error>,
) -> Result<QueryResult, Error> {
    let graveyard = Graveyard::default();
    let patterns = Patterns::default();
    let env = Env::new(
        tx,
        program.slot_count,
        parameters,
        now,
        &graveyard,
        &patterns,
    );
    env.check_closed(&program.closed)?;
    let rows = run(program, |clauses, rows| match clauses {
        [first, rest @ ..] => read(&env, first, rest.first(), rows),
        [] => Ok((rows, 0)),
    })?;
    materialize(&env, &graveyard, &program.columns, rows)
}

/// Runs a program with updating clauses on a write transaction. The caller commits.
pub(crate) fn run_write<W: WriteTransaction>(
    tx: &mut GraphTx<W>,
    program: &Program<Clause>,
    parameters: &[Datum],
    now: &Clocks,
) -> Result<QueryResult, Error> {
    let slots = program.slot_count;
    let mut graveyard = Graveyard::default();
    let patterns = Patterns::default();
    Env::new(&*tx, slots, parameters, now, &graveyard, &patterns).check_closed(&program.closed)?;
    let mut writer = Writer {
        tx,
        slots,
        parameters,
        now,
        patterns: &patterns,
        graveyard: &mut graveyard,
        tokens: Vec::new(),
    };
    let rows = run(program, |clauses, rows| writer.step(clauses, rows))?;
    for node in &graveyard.deleted_nodes {
        tx.ensure_detached(*node)?;
    }
    let env = Env::new(&*tx, slots, parameters, now, &graveyard, &patterns);
    materialize(&env, &graveyard, &program.columns, rows)
}

fn run<C>(
    program: &Program<C>,
    mut apply: impl FnMut(&[C], Vec<Row>) -> Result<(Vec<Row>, usize), Error>,
) -> Result<Vec<Vec<Datum>>, Error> {
    let mut results = Vec::new();
    for part in &program.parts {
        let mut part_rows = Vec::new();
        let mut rows = vec![vec![Datum::Null; program.slot_count]];
        let mut clauses = part.clauses.as_slice();
        while !clauses.is_empty() {
            let (next, applied) = apply(clauses, rows)?;
            rows = next;
            clauses = clauses.get(applied.max(1)..).unwrap_or_default();
        }
        if part.output.is_empty() {
            continue;
        }
        for row in rows {
            part_rows.push(
                part.output
                    .iter()
                    .map(|slot| row.get(*slot).cloned().unwrap_or(Datum::Null))
                    .collect(),
            );
        }
        results.push(part_rows);
    }
    Ok(combine(program.combine, results))
}

fn combine(kind: UnionKind, parts: Vec<Vec<Vec<Datum>>>) -> Vec<Vec<Datum>> {
    let mut parts = parts.into_iter();
    let mut rows = parts.next().unwrap_or_default();
    for next in parts {
        rows = match kind {
            UnionKind::All | UnionKind::Distinct => {
                rows.extend(next);
                rows
            }
            UnionKind::Otherwise if rows.is_empty() => next,
            UnionKind::Otherwise => rows,
            UnionKind::Except { .. } | UnionKind::Intersect { .. } => {
                let mut index = KeyIndex::default();
                let mut counts: Vec<usize> = Vec::new();
                for row in next {
                    match index.get(&row) {
                        Some(at) => counts[at] += 1,
                        None => {
                            index.insert(row, counts.len());
                            counts.push(1);
                        }
                    }
                }
                let keep_found = matches!(kind, UnionKind::Intersect { .. });
                let multiset = matches!(
                    kind,
                    UnionKind::Except { all: true } | UnionKind::Intersect { all: true }
                );
                rows.retain(|row| {
                    let Some(at) = index.get(row) else {
                        return !keep_found;
                    };
                    if !multiset {
                        return keep_found;
                    }
                    match counts[at].checked_sub(1) {
                        Some(left) => {
                            counts[at] = left;
                            keep_found
                        }
                        None => !keep_found,
                    }
                });
                rows
            }
        };
    }
    if matches!(
        kind,
        UnionKind::Distinct
            | UnionKind::Except { all: false }
            | UnionKind::Intersect { all: false }
    ) {
        let mut seen = KeySet::default();
        rows.retain(|row| seen.insert(row));
    }
    rows
}

/// The label whose nodes `found` counts, or none for every node, when the aggregates only count
/// the rows of a match that reads nothing but the count of those nodes.
pub(crate) fn counted_label<'q>(
    found: &'q MatchClause,
    keys: &[(Expr, Slot)],
    aggregates: &[(Aggregate, Slot)],
) -> Option<Option<&'q str>> {
    let (slot, label) = counted_nodes(found)?;
    (keys.is_empty()
        && aggregates
            .iter()
            .all(|(aggregate, _)| aggregate.counts_rows_binding(slot)))
    .then_some(label)
}

/// The values of a branch's output slots in each row it gave.
pub(crate) fn branch_rows<C>(branch: &Pipeline<C>, rows: Vec<Row>) -> Vec<Vec<Datum>> {
    rows.iter()
        .map(|row| {
            branch
                .output
                .iter()
                .map(|slot| row.get(*slot).cloned().unwrap_or(Datum::Null))
                .collect()
        })
        .collect()
}

/// The rows a `table` call gives from the rows of each branch.
pub(crate) fn table_rows<C>(
    call: &CallSubquery<C>,
    parts: Vec<Vec<Vec<Datum>>>,
    slots: usize,
) -> Vec<Row> {
    if call.outputs.is_empty() {
        return vec![vec![Datum::Null; slots]];
    }
    combine(call.combine, parts)
        .into_iter()
        .map(|values| {
            let mut row = vec![Datum::Null; slots];
            for (slot, value) in call.outputs.iter().zip(values) {
                set(&mut row, *slot, value);
            }
            row
        })
        .collect()
}

/// Adds to `out` the rows a `CALL` subquery gives for `row`, from the rows of each branch.
pub(crate) fn extend_row<C>(
    call: &CallSubquery<C>,
    row: Row,
    parts: Vec<Vec<Vec<Datum>>>,
    out: &mut Vec<Row>,
) {
    if call.outputs.is_empty() {
        out.push(row);
        return;
    }
    let results = combine(call.combine, parts);
    if results.is_empty() && call.optional {
        out.push(row);
        return;
    }
    for values in results {
        let mut next = row.clone();
        for (slot, value) in call.outputs.iter().zip(values) {
            set(&mut next, *slot, value);
        }
        out.push(next);
    }
}

fn materialize<T: ReadTransaction>(
    env: &Env<'_, T>,
    graveyard: &Graveyard,
    columns: &[String],
    rows: Vec<Vec<Datum>>,
) -> Result<QueryResult, Error> {
    let rows = rows
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|value| env.value(value, graveyard))
                .collect()
        })
        .collect::<Result<_, _>>()?;
    Ok(QueryResult::new(columns.to_vec(), rows))
}

impl<T: ReadTransaction> Env<'_, T> {
    /// Applies `clause`, and `next` with it when a match feeds an aggregating projection, which
    /// then aggregates each match without collecting the matched rows. Says how many clauses it
    /// applied.
    pub(crate) fn read_clauses(
        &self,
        clause: &ReadClause,
        next: Option<&ReadClause>,
        rows: Vec<Row>,
    ) -> Result<(Vec<Row>, usize), Error> {
        if let (ReadClause::Match(found), Some(ReadClause::Project(projection))) = (clause, next)
            && !found.optional
            && let Items::Aggregate {
                keys,
                aggregates,
                finals,
            } = &projection.items
        {
            let groups = self.match_groups(found, keys, aggregates, &rows)?;
            let rows = groups.finish(self, finals)?;
            return Ok((self.arrange(projection, rows)?, 2));
        }
        if let ReadClause::Match(found) = clause
            && let Some(scan) = self.node_scan(found)?
        {
            if let Some(ReadClause::Project(projection)) = next
                && let Some(rows) = self.scan_project(scan, found, projection, &rows)?
            {
                return Ok((rows, 2));
            }
            if let Some(rows) = self.scan_match(scan, found, &rows)? {
                return Ok((rows, 1));
            }
        }
        Ok((self.read_clause(clause, rows)?, 1))
    }

    /// The groups of the aggregating projection with `keys` and `aggregates` over the matches of
    /// `found` from each of `rows`.
    pub(crate) fn match_groups<'p>(
        &self,
        found: &MatchClause,
        keys: &'p [(Expr, Slot)],
        aggregates: &'p [(Aggregate, Slot)],
        rows: &[Row],
    ) -> Result<Groups<'p>, Error> {
        let mut groups = self.groups(keys, aggregates);
        if let Some(label) = counted_label(found, keys, aggregates) {
            let count = match label {
                None => self.tx.count_nodes()?,
                Some(label) => match self.label_token(label)? {
                    Some(label) => self.tx.count_nodes_with_label(label)?,
                    None => 0,
                },
            };
            let count = usize::try_from(count).unwrap_or(usize::MAX);
            groups.count_rows(count.saturating_mul(rows.len()));
            return Ok(groups);
        }
        if let Some(scan) = self.node_scan(found)?
            && let Some(groups) = self.scan_groups(scan, found, keys, aggregates, rows)?
        {
            return Ok(groups);
        }
        if let Some(groups) = self.hop_groups(found, keys, aggregates, rows)? {
            return Ok(groups);
        }
        for row in rows {
            self.match_patterns(&found.patterns, row, &mut |matched| {
                let keep = match &found.filter {
                    Some(filter) => self.predicate(filter, matched)?,
                    None => true,
                };
                if keep {
                    groups.add(self, matched)?;
                }
                Ok(true)
            })?;
        }
        Ok(groups)
    }

    pub(crate) fn read_clause(
        &self,
        clause: &ReadClause,
        rows: Vec<Row>,
    ) -> Result<Vec<Row>, Error> {
        match clause {
            ReadClause::Match(found) => self.match_clause(found, rows),
            ReadClause::Unwind { expr, slot } => {
                let mut out = Vec::new();
                for row in rows {
                    let items = match self.eval(expr, &row)? {
                        Datum::Null => Arc::default(),
                        Datum::List(items) => items,
                        other => Arc::new(vec![other]),
                    };
                    for item in items.iter() {
                        let mut next = row.clone();
                        set(&mut next, *slot, item.clone());
                        out.push(next);
                    }
                }
                Ok(out)
            }
            ReadClause::Project(projection) => self.project(projection, rows),
            ReadClause::Call(call) => self.call_procedure(call, rows),
            ReadClause::Filter(condition) => {
                let mut kept = Vec::new();
                for row in rows {
                    if self.predicate(condition, &row)? {
                        kept.push(row);
                    }
                }
                Ok(kept)
            }
            ReadClause::CallSubquery(call) => self.call_subquery(call, rows),
            ReadClause::Let(bindings) => rows
                .into_iter()
                .map(|mut row| {
                    for (expr, slot) in bindings {
                        let value = self.eval(expr, &row)?;
                        set(&mut row, *slot, value);
                    }
                    Ok(row)
                })
                .collect(),
        }
    }

    fn call_subquery(
        &self,
        call: &CallSubquery<ReadClause>,
        rows: Vec<Row>,
    ) -> Result<Vec<Row>, Error> {
        if call.table {
            let mut parts = Vec::new();
            for branch in &call.branches {
                let mut inner = rows.clone();
                for clause in &branch.clauses {
                    inner = self.read_clause(clause, inner)?;
                }
                parts.push(branch_rows(branch, inner));
            }
            return Ok(table_rows(call, parts, self.slots));
        }
        let mut out = Vec::new();
        for row in rows {
            let mut parts = Vec::new();
            for branch in &call.branches {
                let mut inner = vec![row.clone()];
                for clause in &branch.clauses {
                    inner = self.read_clause(clause, inner)?;
                }
                parts.push(branch_rows(branch, inner));
            }
            extend_row(call, row, parts, &mut out);
        }
        Ok(out)
    }

    fn call_procedure(&self, call: &CallClause, rows: Vec<Row>) -> Result<Vec<Row>, Error> {
        let signature = call.callee.signature();
        let mut out = Vec::new();
        for row in rows {
            let mut args = Vec::new();
            for (arg, (parameter, accepts)) in call.args.iter().zip(&signature.parameters) {
                let value = match self.eval(arg, &row)? {
                    Datum::Integer(value) if *accepts == ValueType::Float => {
                        Datum::Float(value as f64)
                    }
                    value => value,
                };
                if let Some(shape) = Shape::of(&value)
                    && !admits(procedure_parameter(*accepts), shape)
                {
                    return Err(Error::rejecting_type(
                        ErrorKind::InvalidArgumentValue,
                        format!(
                            "parameter `{parameter}` of `{}` cannot take a {}",
                            signature.name,
                            value.type_name()
                        ),
                    ));
                }
                args.push(value);
            }
            let results = match &call.callee {
                Callee::Host(procedure) => self.call_host(procedure, args)?,
                Callee::Builtin(builtin) => builtin.call(self.tx, &args)?,
            };
            if signature.outputs.is_empty() {
                out.push(row);
                continue;
            }
            for result in results {
                if result.len() != signature.outputs.len() {
                    return Err(Error::new(
                        ErrorKind::ProcedureFailed,
                        format!(
                            "procedure `{}` returned a row of {} values for {} outputs",
                            signature.name,
                            result.len(),
                            signature.outputs.len()
                        ),
                    ));
                }
                let mut next = row.clone();
                for (index, slot) in &call.yields {
                    if let Some(value) = result.get(*index) {
                        set(&mut next, *slot, value.clone());
                    }
                }
                let keep = match &call.filter {
                    Some(filter) => self.predicate(filter, &next)?,
                    None => true,
                };
                if keep {
                    out.push(next);
                }
            }
        }
        Ok(out)
    }

    fn call_host(&self, procedure: &Procedure, args: Vec<Datum>) -> Result<Vec<Vec<Datum>>, Error> {
        let args = args
            .into_iter()
            .map(|arg| self.value(arg, self.graveyard))
            .collect::<Result<Vec<_>, _>>()?;
        let results = (procedure.body)(&args)
            .map_err(|source| {
                Error::caused_by(
                    ErrorKind::ProcedureFailed,
                    format!("procedure `{}` failed: {source}", procedure.signature.name),
                    source,
                )
            })?
            .into_rows();
        let rows: Result<Vec<Vec<Datum>>, Error> = results
            .iter()
            .map(|result| result.iter().map(from_parameter).collect())
            .collect();
        if rows.is_err() {
            dismantle(results.into_iter().flatten().collect());
        }
        rows
    }

    fn match_clause(&self, found: &MatchClause, rows: Vec<Row>) -> Result<Vec<Row>, Error> {
        let mut out = Vec::new();
        for mut row in rows {
            let before = out.len();
            let sought = found.by_id.as_deref().and_then(|by_id| {
                let value = self.eval(&by_id.value, &row).ok()?;
                Some((by_id, value))
            });
            match sought {
                Some((by_id, value)) => {
                    if let (Some(id), [part]) = (seek::id(&value), by_id.clause.patterns.as_slice())
                    {
                        let id = NodeId(id);
                        set(&mut row, part.start.slot, Datum::Node(id));
                        if part.node_only()
                            && part.path.is_none()
                            && part.start.labels.is_none()
                            && part.start.properties.is_none()
                        {
                            let kept = self.with_labels(id, |_| ())?.is_some()
                                && match &by_id.clause.filter {
                                    Some(filter) => self.predicate(filter, &row)?,
                                    None => true,
                                };
                            if kept {
                                out.push(row);
                                continue;
                            }
                        } else {
                            self.match_into(&by_id.clause, &row, &mut out)?;
                        }
                    }
                }
                None => self.match_into(found, &row, &mut out)?,
            }
            if found.optional && out.len() == before {
                let mut padded = row;
                for slot in &found.introduced {
                    set(&mut padded, *slot, Datum::Null);
                }
                out.push(padded);
            }
        }
        Ok(out)
    }

    fn match_into(&self, found: &MatchClause, row: &Row, out: &mut Vec<Row>) -> Result<(), Error> {
        self.match_patterns(&found.patterns, row, &mut |matched| {
            let keep = match &found.filter {
                Some(filter) => self.predicate(filter, matched)?,
                None => true,
            };
            if keep {
                out.push(matched.clone());
            }
            Ok(true)
        })
    }

    fn project(&self, projection: &Projection, rows: Vec<Row>) -> Result<Vec<Row>, Error> {
        let rows = match &projection.items {
            Items::Plain(items) => {
                let mut rows = rows;
                for row in &mut rows {
                    for (expr, slot) in items {
                        let value = self.eval(expr, row)?;
                        set(row, *slot, value);
                    }
                }
                rows
            }
            Items::Aggregate {
                keys,
                aggregates,
                finals,
            } => self.aggregate(rows, keys, aggregates, finals)?,
        };
        self.arrange(projection, rows)
    }

    pub(crate) fn arrange(
        &self,
        projection: &Projection,
        mut rows: Vec<Row>,
    ) -> Result<Vec<Row>, Error> {
        if projection.distinct {
            let mut seen = KeySet::default();
            rows.retain(|row| {
                seen.insert(
                    &projection
                        .outputs
                        .iter()
                        .map(|slot| row.get(*slot).cloned().unwrap_or(Datum::Null))
                        .collect::<Vec<_>>(),
                )
            });
        }
        let empty = || vec![Datum::Null; self.slots];
        let skip = match &projection.skip {
            Some(skip) => self.count(skip, &empty(), "SKIP")?,
            None => 0,
        };
        let limit = match &projection.limit {
            Some(limit) => Some(self.count(limit, &empty(), "LIMIT")?),
            None => None,
        };
        if !projection.order_by.is_empty() {
            let kept = limit.map(|limit| skip.saturating_add(limit));
            rows = self.sort(rows, &projection.order_by, kept)?;
        }
        if skip > 0 {
            rows = rows.into_iter().skip(skip).collect();
        }
        if let Some(limit) = limit {
            rows.truncate(limit);
        }
        if let Some(filter) = &projection.filter {
            let mut kept = Vec::new();
            for row in rows {
                if self.predicate(filter, &row)? {
                    kept.push(row);
                }
            }
            rows = kept;
        }
        Ok(rows)
    }

    fn check_closed(&self, closed: &[Expr]) -> Result<(), Error> {
        let check = Check::default();
        let checker = Env {
            check: Some(&check),
            ..Env::new(
                self.tx,
                self.slots,
                self.parameters,
                self.now,
                self.graveyard,
                self.patterns,
            )
        };
        let row = vec![Datum::Null; self.slots];
        for expr in closed {
            if let Err(error) = checker.eval(expr, &row) {
                return Err(match error.kind() {
                    ErrorKind::TypeMismatch => error.with_kind(ErrorKind::InvalidArgumentType),
                    _ => error,
                });
            }
        }
        Ok(())
    }

    pub(crate) fn count(&self, expr: &Expr, row: &Row, clause: &str) -> Result<usize, Error> {
        match self.eval(expr, row)? {
            Datum::Integer(value) if value < 0 => Err(Error::new(
                ErrorKind::NegativeIntegerArgument,
                format!("{clause} cannot be negative"),
            )),
            Datum::Integer(value) => Ok(usize::try_from(value).unwrap_or(usize::MAX)),
            other => Err(Error::new(
                ErrorKind::InvalidArgumentType,
                format!("{clause} needs an integer, not a {}", other.type_name()),
            )),
        }
    }

    fn sort(
        &self,
        rows: Vec<Row>,
        order_by: &[(Expr, SortDirection)],
        kept: Option<usize>,
    ) -> Result<Vec<Row>, Error> {
        let mut keyed = Vec::with_capacity(rows.len());
        for (position, row) in rows.into_iter().enumerate() {
            let keys = order_by
                .iter()
                .map(|(expr, _)| self.eval(expr, &row))
                .collect::<Result<Vec<_>, _>>()?;
            keyed.push((keys, position, row));
        }
        let compare = |(a, a_position, _): &(Vec<Datum>, usize, Row),
                       (b, b_position, _): &(Vec<Datum>, usize, Row)| {
            for ((x, y), (_, direction)) in a.iter().zip(b).zip(order_by) {
                let ordering = sort_order(*direction, x, y);
                if ordering != Ordering::Equal {
                    return ordering;
                }
            }
            a_position.cmp(b_position)
        };
        if let Some(kept) = kept
            && kept < keyed.len()
        {
            match kept.checked_sub(1) {
                Some(last) => {
                    keyed.select_nth_unstable_by(last, compare);
                    keyed.truncate(kept);
                }
                None => keyed.clear(),
            }
        }
        keyed.sort_unstable_by(compare);
        Ok(keyed.into_iter().map(|(_, _, row)| row).collect())
    }

    /// Turns a runtime value into a public value, reading graph elements as they are now and
    /// deleted ones as they were when deleted.
    pub(crate) fn value(&self, value: Datum, graveyard: &Graveyard) -> Result<Value, Error> {
        Ok(match value {
            Datum::Null => Value::Null,
            Datum::Boolean(value) => Value::Boolean(value),
            Datum::Integer(value) => Value::Integer(value),
            Datum::Float(value) => Value::Float(value),
            Datum::String(value) => Value::String(value),
            Datum::Bytes(value) => Value::Bytes(value),
            Datum::Temporal(value) => Value::Temporal(value),
            Datum::List(items) => {
                Value::List(self.list_value(Arc::unwrap_or_clone(items), graveyard)?)
            }
            Datum::Map(entries) => {
                Value::Map(self.map_value(Arc::unwrap_or_clone(entries), graveyard)?)
            }
            Datum::Node(id) => Value::Node(self.node_value(id, graveyard)?),
            Datum::Relationship(id) => Value::Relationship(self.relationship_value(id, graveyard)?),
            Datum::Path(path) => Value::Path(self.path_value(path, graveyard)?),
        })
    }

    fn list_value(&self, items: Vec<Datum>, graveyard: &Graveyard) -> Result<Vec<Value>, Error> {
        let mut values = Vec::with_capacity(items.len());
        for item in items {
            values.push(self.value(item, graveyard)?);
        }
        Ok(values)
    }

    fn map_value(
        &self,
        entries: BTreeMap<Arc<str>, Datum>,
        graveyard: &Graveyard,
    ) -> Result<BTreeMap<String, Value>, Error> {
        let mut values = BTreeMap::new();
        for (key, value) in entries {
            values.insert(key.to_string(), self.value(value, graveyard)?);
        }
        Ok(values)
    }

    fn path_value(&self, path: PathIds, graveyard: &Graveyard) -> Result<result::Path, Error> {
        Ok(result::Path::new(
            path.nodes
                .into_iter()
                .map(|id| self.node_value(id, graveyard))
                .collect::<Result<_, _>>()?,
            path.relationships
                .into_iter()
                .map(|id| self.relationship_value(id, graveyard))
                .collect::<Result<_, _>>()?,
        ))
    }

    fn node_value(
        &self,
        id: crate::graph::NodeId,
        graveyard: &Graveyard,
    ) -> Result<result::Node, Error> {
        let record = match graveyard.nodes.get(&id) {
            Some(record) => record.clone(),
            None => self.node(id)?,
        };
        Ok(result::Node {
            id,
            labels: self.label_names(&record)?.into_iter().collect(),
            properties: self.map_value(self.property_map(&record.properties)?, graveyard)?,
        })
    }

    fn relationship_value(
        &self,
        id: crate::graph::RelationshipId,
        graveyard: &Graveyard,
    ) -> Result<result::Relationship, Error> {
        let record = match graveyard.relationships.get(&id) {
            Some(record) => record.clone(),
            None => self.relationship(id)?,
        };
        Ok(result::Relationship {
            id,
            relationship_type: self.tx.token_name(record.rel_type)?,
            start: record.source,
            end: record.target,
            properties: self.map_value(self.property_map(&record.properties)?, graveyard)?,
        })
    }
}
