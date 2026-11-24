use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashSet;

use crate::error::Error;
use crate::error::ErrorKind;
use crate::graph::NodeId;
use crate::graph::RelationshipId;
use crate::storage::ReadTransaction;

use super::compare::Key;
use super::compare::order;
use super::datum;
use super::datum::Datum;
use super::eval::Env;
use super::eval::Row;
use super::eval::set;
use super::eval::type_error;
use super::plan::Aggregate;
use super::plan::AggregateFunction;
use super::plan::Expr;
use super::plan::Slot;
use super::shape::NUMBER;
use super::shape::Shape;
use super::shape::admits;

#[derive(Default)]
struct Seen {
    nodes: HashSet<NodeId>,
    relationships: HashSet<RelationshipId>,
    others: BTreeSet<Key>,
}

impl Seen {
    fn insert(&mut self, value: &Datum) -> bool {
        match value {
            Datum::Node(id) => self.nodes.insert(*id),
            Datum::Relationship(id) => self.relationships.insert(*id),
            other => self.others.insert(Key(vec![other.clone()])),
        }
    }
}

/// The groups an aggregating projection has seen so far, each with the state of every
/// aggregate. Rows are added one at a time, so a match can feed them without collecting rows.
pub(crate) struct Groups<'p> {
    keys: &'p [(Expr, Slot)],
    aggregates: &'p [(Aggregate, Slot)],
    index: BTreeMap<Key, usize>,
    groups: Vec<(Vec<Datum>, Vec<Accumulator>)>,
}

#[derive(Default)]
struct Accumulator {
    rows: usize,
    counted: usize,
    values: Vec<Datum>,
    seen: Seen,
    percentile: Option<Datum>,
}

impl<'p> Groups<'p> {
    pub(crate) fn new(keys: &'p [(Expr, Slot)], aggregates: &'p [(Aggregate, Slot)]) -> Self {
        Self {
            keys,
            aggregates,
            index: BTreeMap::new(),
            groups: Vec::new(),
        }
    }

    pub(crate) fn add<T: ReadTransaction>(
        &mut self,
        env: &Env<'_, T>,
        row: &Row,
    ) -> Result<(), Error> {
        let values = self
            .keys
            .iter()
            .map(|(expr, _)| env.eval(expr, row))
            .collect::<Result<Vec<_>, _>>()?;
        let key = Key(values);
        let position = match self.index.get(&key) {
            Some(position) => *position,
            None => {
                let position = self.groups.len();
                let accumulators = self
                    .aggregates
                    .iter()
                    .map(|_| Accumulator::default())
                    .collect();
                self.groups.push((key.0.clone(), accumulators));
                self.index.insert(key, position);
                position
            }
        };
        if let Some((_, accumulators)) = self.groups.get_mut(position) {
            for ((aggregate, _), accumulator) in self.aggregates.iter().zip(accumulators) {
                accumulator.add(env, aggregate, row)?;
            }
        }
        Ok(())
    }

    /// Adds `rows` rows to the one group of a projection without keys whose aggregates all
    /// [count rows](Aggregate::counts_rows_binding) whose counted slot is never null, so they
    /// need not be read.
    pub(crate) fn count_rows(&mut self, rows: usize) {
        if self.groups.is_empty() {
            let accumulators = self
                .aggregates
                .iter()
                .map(|_| Accumulator::default())
                .collect();
            self.groups.push((Vec::new(), accumulators));
        }
        for (_, accumulators) in &mut self.groups {
            for accumulator in accumulators {
                accumulator.rows += rows;
                accumulator.counted += rows;
            }
        }
    }

    /// One row per group with keys, aggregates, and final expressions filled in. Without keys,
    /// even no rows make one group.
    pub(crate) fn finish<T: ReadTransaction>(
        mut self,
        env: &Env<'_, T>,
        finals: &[(Expr, Slot)],
    ) -> Result<Vec<Row>, Error> {
        if self.keys.is_empty() && self.groups.is_empty() {
            let accumulators = self
                .aggregates
                .iter()
                .map(|_| Accumulator::default())
                .collect();
            self.groups.push((Vec::new(), accumulators));
        }
        let mut out = Vec::with_capacity(self.groups.len());
        for (values, accumulators) in self.groups {
            let mut row = vec![Datum::Null; env.slots];
            for ((_, slot), value) in self.keys.iter().zip(values) {
                set(&mut row, *slot, value);
            }
            for ((aggregate, slot), accumulator) in self.aggregates.iter().zip(accumulators) {
                set(&mut row, *slot, accumulator.finish(aggregate)?);
            }
            for (expr, slot) in finals {
                let value = env.eval(expr, &row)?;
                set(&mut row, *slot, value);
            }
            out.push(row);
        }
        Ok(out)
    }
}

impl<T: ReadTransaction> Env<'_, T> {
    /// Groups `rows` by the key values and yields one row per group with keys, aggregates, and
    /// final expressions filled in. Without keys, even no rows make one group.
    pub(crate) fn aggregate(
        &self,
        rows: Vec<Row>,
        keys: &[(Expr, Slot)],
        aggregates: &[(Aggregate, Slot)],
        finals: &[(Expr, Slot)],
    ) -> Result<Vec<Row>, Error> {
        let mut groups = Groups::new(keys, aggregates);
        for row in &rows {
            groups.add(self, row)?;
        }
        groups.finish(self, finals)
    }
}

impl Accumulator {
    fn add<T: ReadTransaction>(
        &mut self,
        env: &Env<'_, T>,
        aggregate: &Aggregate,
        row: &Row,
    ) -> Result<(), Error> {
        self.rows += 1;
        let Some(argument) = &aggregate.argument else {
            return Ok(());
        };
        let function = aggregate.function;
        let value = env.eval(argument, row)?;
        if !value.is_null() {
            function.admit(&value)?;
            if !aggregate.distinct || self.seen.insert(&value) {
                if function == AggregateFunction::Count {
                    self.counted += 1;
                } else {
                    self.values.push(value);
                }
            }
        }
        if let Some(extra) = &aggregate.extra
            && matches!(
                function,
                AggregateFunction::PercentileCont | AggregateFunction::PercentileDisc
            )
        {
            let value = env.eval(extra, row)?;
            function.admit(&value)?;
            match &self.percentile {
                Some(first) if order(first, &value) != Ordering::Equal => {
                    return Err(Error::new(
                        ErrorKind::InvalidArgumentValue,
                        "the percentile must be the same for every row of a group",
                    ));
                }
                Some(_) => {}
                None => self.percentile = Some(value),
            }
        }
        Ok(())
    }

    fn finish(self, aggregate: &Aggregate) -> Result<Datum, Error> {
        let values = self.values;
        match aggregate.function {
            AggregateFunction::Count => {
                let count = match aggregate.argument {
                    Some(_) => self.counted,
                    None => self.rows,
                };
                Ok(Datum::Integer(i64::try_from(count).unwrap_or(i64::MAX)))
            }
            AggregateFunction::Collect => datum::list(values),
            AggregateFunction::Min => Ok(values.into_iter().min_by(order).unwrap_or(Datum::Null)),
            AggregateFunction::Max => Ok(values.into_iter().max_by(order).unwrap_or(Datum::Null)),
            AggregateFunction::Sum => sum(&values),
            AggregateFunction::Avg => {
                if values.is_empty() {
                    return Ok(Datum::Null);
                }
                let numbers = floats(&values)?;
                Ok(Datum::Float(
                    numbers.iter().sum::<f64>() / numbers.len() as f64,
                ))
            }
            AggregateFunction::StDev | AggregateFunction::StDevP => {
                let numbers = floats(&values)?;
                let sample = aggregate.function == AggregateFunction::StDev;
                let count = numbers.len() as f64;
                let divisor = if sample { count - 1.0 } else { count };
                if divisor <= 0.0 {
                    return Ok(Datum::Float(0.0));
                }
                let mean = numbers.iter().sum::<f64>() / count;
                let squares: f64 = numbers.iter().map(|value| (value - mean).powi(2)).sum();
                Ok(Datum::Float((squares / divisor).sqrt()))
            }
            AggregateFunction::PercentileCont | AggregateFunction::PercentileDisc => {
                let percentile = match &aggregate.extra {
                    Some(_) => self.percentile,
                    None => Some(Datum::Null),
                };
                match percentile {
                    Some(percentile) => percentile_of(aggregate.function, values, &percentile),
                    None => Ok(Datum::Null),
                }
            }
        }
    }
}

impl AggregateFunction {
    /// The shapes every argument accepts, where none means any. The binder checks the arguments
    /// whose shape the query text decides against them, and the computation checks the values it
    /// gets against them, so an aggregate takes the same types whichever way they arrive.
    pub(crate) fn accepted(self) -> &'static [Shape] {
        match self {
            AggregateFunction::Sum
            | AggregateFunction::Avg
            | AggregateFunction::StDev
            | AggregateFunction::StDevP
            | AggregateFunction::PercentileCont
            | AggregateFunction::PercentileDisc => NUMBER,
            AggregateFunction::Count
            | AggregateFunction::Min
            | AggregateFunction::Max
            | AggregateFunction::Collect => &[],
        }
    }

    pub(crate) fn admit(self, value: &Datum) -> Result<(), Error> {
        match Shape::of(value) {
            Some(shape) if !admits(self.accepted(), shape) => Err(type_error(format!(
                "{self:?} cannot take a {}",
                value.type_name()
            ))),
            _ => Ok(()),
        }
    }
}

fn floats(values: &[Datum]) -> Result<Vec<f64>, Error> {
    values
        .iter()
        .map(|value| match value {
            Datum::Integer(value) => Ok(*value as f64),
            Datum::Float(value) => Ok(*value),
            other => Err(type_error(format!(
                "expected a number, got a {}",
                other.type_name()
            ))),
        })
        .collect()
}

fn sum(values: &[Datum]) -> Result<Datum, Error> {
    if values
        .iter()
        .all(|value| matches!(value, Datum::Integer(_)))
    {
        let mut total: i64 = 0;
        for value in values {
            if let Datum::Integer(value) = value {
                total = total
                    .checked_add(*value)
                    .ok_or_else(|| Error::new(ErrorKind::IntegerOverflow, "sum overflowed"))?;
            }
        }
        return Ok(Datum::Integer(total));
    }
    Ok(Datum::Float(floats(values)?.iter().sum()))
}

fn percentile_of(
    function: AggregateFunction,
    values: Vec<Datum>,
    percentile: &Datum,
) -> Result<Datum, Error> {
    let percentile = match percentile {
        Datum::Integer(value) => *value as f64,
        Datum::Float(value) => *value,
        Datum::Null => return Ok(Datum::Null),
        other => {
            return Err(type_error(format!(
                "a percentile must be a number, not a {}",
                other.type_name()
            )));
        }
    };
    if !(0.0..=1.0).contains(&percentile) {
        return Err(Error::new(
            ErrorKind::NumberOutOfRange,
            "a percentile must be between 0.0 and 1.0",
        ));
    }
    let mut numbers = floats(&values)?;
    if numbers.is_empty() {
        return Ok(Datum::Null);
    }
    numbers.sort_by(f64::total_cmp);
    let mut sorted = values;
    sorted.sort_by(order);
    let last = numbers.len() - 1;
    if function == AggregateFunction::PercentileDisc {
        let position = (percentile * numbers.len() as f64).ceil() as usize;
        let index = position.saturating_sub(1).min(last);
        return Ok(sorted.swap_remove(index));
    }
    let exact = percentile * last as f64;
    let lower = exact.floor() as usize;
    let upper = exact.ceil() as usize;
    let fraction = exact - lower as f64;
    Ok(Datum::Float(
        numbers[lower] + (numbers[upper.min(last)] - numbers[lower]) * fraction,
    ))
}
