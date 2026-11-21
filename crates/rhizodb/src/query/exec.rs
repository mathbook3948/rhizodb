use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

use crate::error::Error;
use crate::error::ErrorKind;
use crate::gql::ast::SortDirection;
use crate::graph::GraphTx;
use crate::result;
use crate::result::QueryResult;
use crate::result::Value;
use crate::storage::ReadTransaction;
use crate::storage::WriteTransaction;

use super::compare::Key;
use super::compare::order;
use super::datum::Datum;
use super::eval::Env;
use super::eval::Row;
use super::eval::set;
use super::plan::Clause;
use super::plan::Expr;
use super::plan::Items;
use super::plan::MatchClause;
use super::plan::Program;
use super::plan::Projection;
use super::plan::ReadClause;
use super::update::Graveyard;

/// Runs a read-only program. It only needs a read transaction.
pub(crate) fn run_read<T: ReadTransaction>(
    tx: &GraphTx<T>,
    program: &Program<ReadClause>,
) -> Result<QueryResult, Error> {
    let env = Env {
        tx,
        slots: program.slot_count,
    };
    env.check_closed(&program.closed)?;
    let rows = run(program, |clause, rows| env.read_clause(clause, rows))?;
    materialize(&env, &Graveyard::default(), &program.columns, rows)
}

/// Runs a program with updating clauses on a write transaction. The caller commits.
pub(crate) fn run_write<W: WriteTransaction>(
    tx: &mut GraphTx<W>,
    program: &Program<Clause>,
) -> Result<QueryResult, Error> {
    let slots = program.slot_count;
    Env { tx: &*tx, slots }.check_closed(&program.closed)?;
    let mut graveyard = Graveyard::default();
    let rows = run(program, |clause, rows| match clause {
        Clause::Read(clause) => Env { tx: &*tx, slots }.read_clause(clause, rows),
        Clause::Update(clause) => super::update::apply(tx, slots, &mut graveyard, clause, rows),
    })?;
    materialize(&Env { tx: &*tx, slots }, &graveyard, &program.columns, rows)
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
    fn read_clause(&self, clause: &ReadClause, rows: Vec<Row>) -> Result<Vec<Row>, Error> {
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
        }
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
        let row = vec![Datum::Null; self.slots];
        for expr in closed {
            if let Err(error) = self.eval(expr, &row)
                && error.kind() == ErrorKind::TypeMismatch
            {
                return Err(error.with_kind(ErrorKind::InvalidArgumentType));
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
