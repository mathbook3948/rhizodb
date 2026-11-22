use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

use crate::error::Error;
use crate::error::ErrorKind;
use crate::gql::ast::SortDirection;
use crate::graph::GraphTx;
use crate::procedure::Procedure;
use crate::procedure::ValueType;
use crate::result;
use crate::result::QueryResult;
use crate::result::Value;
use crate::storage::ReadTransaction;
use crate::storage::WriteTransaction;
use crate::temporal::Clocks;

use super::compare::Key;
use super::compare::order;
use super::datum::Datum;
use super::datum::from_parameter;
use super::eval::Check;
use super::eval::Env;
use super::eval::Row;
use super::eval::set;
use super::plan::CallClause;
use super::plan::Callee;
use super::plan::Clause;
use super::plan::Expr;
use super::plan::Items;
use super::plan::MatchClause;
use super::plan::Program;
use super::plan::Projection;
use super::plan::ReadClause;
use super::regex::Patterns;
use super::shape::Shape;
use super::shape::admits;
use super::shape::procedure_parameter;
use super::update::Graveyard;

/// Runs a read-only program. It only needs a read transaction.
pub(crate) fn run_read<T: ReadTransaction>(
    tx: &GraphTx<T>,
    program: &Program<ReadClause>,
    now: &Clocks,
) -> Result<QueryResult, Error> {
    let graveyard = Graveyard::default();
    let patterns = Patterns::default();
    let env = Env {
        tx,
        slots: program.slot_count,
        now,
        graveyard: &graveyard,
        patterns: &patterns,
        check: None,
    };
    env.check_closed(&program.closed)?;
    let rows = run(program, |clause, rows| env.read_clause(clause, rows))?;
    materialize(&env, &graveyard, &program.columns, rows)
}

/// Runs a program with updating clauses on a write transaction. The caller commits.
pub(crate) fn run_write<W: WriteTransaction>(
    tx: &mut GraphTx<W>,
    program: &Program<Clause>,
    now: &Clocks,
) -> Result<QueryResult, Error> {
    let slots = program.slot_count;
    let mut graveyard = Graveyard::default();
    let patterns = Patterns::default();
    Env {
        tx: &*tx,
        slots,
        now,
        graveyard: &graveyard,
        patterns: &patterns,
        check: None,
    }
    .check_closed(&program.closed)?;
    let rows = run(program, |clause, rows| match clause {
        Clause::Read(clause) => Env {
            tx: &*tx,
            slots,
            now,
            graveyard: &graveyard,
            patterns: &patterns,
            check: None,
        }
        .read_clause(clause, rows),
        Clause::Update(clause) => {
            super::update::apply(tx, slots, now, &patterns, &mut graveyard, clause, rows)
        }
    })?;
    for node in &graveyard.deleted_nodes {
        tx.ensure_detached(*node)?;
    }
    let env = Env {
        tx: &*tx,
        slots,
        now,
        graveyard: &graveyard,
        patterns: &patterns,
        check: None,
    };
    materialize(&env, &graveyard, &program.columns, rows)
}

fn run<C>(
    program: &Program<C>,
    mut apply: impl FnMut(&C, Vec<Row>) -> Result<Vec<Row>, Error>,
) -> Result<Vec<Vec<Datum>>, Error> {
    let mut results = Vec::new();
    for part in &program.parts {
        let mut rows = vec![vec![Datum::Null; program.slot_count]];
        for clause in &part.clauses {
            rows = apply(clause, rows)?;
        }
        if part.output.is_empty() {
            continue;
        }
        for row in rows {
            results.push(
                part.output
                    .iter()
                    .map(|slot| row.get(*slot).cloned().unwrap_or(Datum::Null))
                    .collect(),
            );
        }
    }
    if program.distinct {
        let mut seen = BTreeSet::new();
        results.retain(|row: &Vec<Datum>| seen.insert(Key(row.clone())));
    }
    Ok(results)
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
                        Datum::Null => Vec::new(),
                        Datum::List(items) => items,
                        other => vec![other],
                    };
                    for item in items {
                        let mut next = row.clone();
                        set(&mut next, *slot, item);
                        out.push(next);
                    }
                }
                Ok(out)
            }
            ReadClause::Project(projection) => self.project(projection, rows),
            ReadClause::Call(call) => self.call_procedure(call, rows),
        }
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
        let results = (procedure.body)(&args).map_err(|source| {
            Error::caused_by(
                ErrorKind::ProcedureFailed,
                format!("procedure `{}` failed: {source}", procedure.signature.name),
                source,
            )
        })?;
        Ok(results
            .iter()
            .map(|result| result.iter().map(from_parameter).collect())
            .collect())
    }

    fn match_clause(&self, found: &MatchClause, rows: Vec<Row>) -> Result<Vec<Row>, Error> {
        let mut out = Vec::new();
        for row in rows {
            let before = out.len();
            self.match_patterns(&found.patterns, &row, &mut |matched| {
                let keep = match &found.filter {
                    Some(filter) => self.predicate(filter, matched)?,
                    None => true,
                };
                if keep {
                    out.push(matched.clone());
                }
                Ok(true)
            })?;
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

    fn project(&self, projection: &Projection, rows: Vec<Row>) -> Result<Vec<Row>, Error> {
        let mut rows = match &projection.items {
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
        if projection.distinct {
            let mut seen = BTreeSet::new();
            rows.retain(|row| {
                seen.insert(Key(projection
                    .outputs
                    .iter()
                    .map(|slot| row.get(*slot).cloned().unwrap_or(Datum::Null))
                    .collect()))
            });
        }
        if !projection.order_by.is_empty() {
            rows = self.sort(rows, &projection.order_by)?;
        }
        let empty = vec![Datum::Null; self.slots];
        if let Some(skip) = &projection.skip {
            let skip = self.count(skip, &empty, "SKIP")?;
            rows = rows.into_iter().skip(skip).collect();
        }
        if let Some(limit) = &projection.limit {
            let limit = self.count(limit, &empty, "LIMIT")?;
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
            tx: self.tx,
            slots: self.slots,
            now: self.now,
            graveyard: self.graveyard,
            patterns: self.patterns,
            check: Some(&check),
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

    fn count(&self, expr: &Expr, row: &Row, clause: &str) -> Result<usize, Error> {
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

    fn sort(&self, rows: Vec<Row>, order_by: &[(Expr, SortDirection)]) -> Result<Vec<Row>, Error> {
        let mut keyed = Vec::with_capacity(rows.len());
        for row in rows {
            let keys = order_by
                .iter()
                .map(|(expr, _)| self.eval(expr, &row))
                .collect::<Result<Vec<_>, _>>()?;
            keyed.push((keys, row));
        }
        keyed.sort_by(|(a, _), (b, _)| {
            for ((x, y), (_, direction)) in a.iter().zip(b).zip(order_by) {
                let ordering = order(x, y);
                let ordering = match direction {
                    SortDirection::Ascending => ordering,
                    SortDirection::Descending => ordering.reverse(),
                };
                if ordering != Ordering::Equal {
                    return ordering;
                }
            }
            Ordering::Equal
        });
        Ok(keyed.into_iter().map(|(_, row)| row).collect())
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
            Datum::List(items) => Value::List(
                items
                    .into_iter()
                    .map(|item| self.value(item, graveyard))
                    .collect::<Result<_, _>>()?,
            ),
            Datum::Map(entries) => Value::Map(self.map_value(entries, graveyard)?),
            Datum::Node(id) => Value::Node(self.node_value(id, graveyard)?),
            Datum::Relationship(id) => Value::Relationship(self.relationship_value(id, graveyard)?),
            Datum::Path(path) => Value::Path(result::Path::new(
                path.nodes
                    .into_iter()
                    .map(|id| self.node_value(id, graveyard))
                    .collect::<Result<_, _>>()?,
                path.relationships
                    .into_iter()
                    .map(|id| self.relationship_value(id, graveyard))
                    .collect::<Result<_, _>>()?,
            )),
        })
    }

    fn map_value(
        &self,
        entries: BTreeMap<String, Datum>,
        graveyard: &Graveyard,
    ) -> Result<BTreeMap<String, Value>, Error> {
        entries
            .into_iter()
            .map(|(key, value)| Ok((key, self.value(value, graveyard)?)))
            .collect()
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
            rel_type: self.tx.token_name(record.rel_type)?,
            start: record.source,
            end: record.target,
            properties: self.map_value(self.property_map(&record.properties)?, graveyard)?,
        })
    }
}
