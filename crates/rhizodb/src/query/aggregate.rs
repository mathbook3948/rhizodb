use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

use crate::error::Error;
use crate::error::ErrorKind;
use crate::storage::ReadTransaction;

use super::compare::Key;
use super::compare::order;
use super::datum::Datum;
use super::eval::Env;
use super::eval::Row;
use super::eval::set;
use super::eval::type_error;
use super::plan::Aggregate;
use super::plan::AggregateFunction;
use super::plan::Expr;
use super::plan::Slot;

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
        let mut groups: Vec<(Vec<Datum>, Vec<Row>)> = Vec::new();
        let mut index: BTreeMap<Key, usize> = BTreeMap::new();
        for row in rows {
            let values = keys
                .iter()
                .map(|(expr, _)| self.eval(expr, &row))
                .collect::<Result<Vec<_>, _>>()?;
            let key = Key(values.clone());
            match index.get(&key) {
                Some(position) => groups[*position].1.push(row),
                None => {
                    index.insert(key, groups.len());
                    groups.push((values, vec![row]));
                }
            }
        }
        if keys.is_empty() && groups.is_empty() {
            groups.push((Vec::new(), Vec::new()));
        }
        let mut out = Vec::with_capacity(groups.len());
        for (values, members) in groups {
            let mut row = vec![Datum::Null; self.slots];
            for ((_, slot), value) in keys.iter().zip(values) {
                set(&mut row, *slot, value);
            }
            for (aggregate, slot) in aggregates {
                let value = self.compute(aggregate, &members)?;
                set(&mut row, *slot, value);
            }
            for (expr, slot) in finals {
                let value = self.eval(expr, &row)?;
                set(&mut row, *slot, value);
            }
            out.push(row);
        }
        Ok(out)
    }

    fn group_percentile(&self, extra: &Expr, rows: &[Row]) -> Result<Datum, Error> {
        let mut percentile: Option<Datum> = None;
        for row in rows {
            let value = self.eval(extra, row)?;
            match &percentile {
                Some(first) if order(first, &value) != Ordering::Equal => {
                    return Err(Error::new(
                        ErrorKind::InvalidArgumentValue,
                        "the percentile must be the same for every row of a group",
                    ));
                }
                Some(_) => {}
                None => percentile = Some(value),
            }
        }
        match percentile {
            Some(percentile) => Ok(percentile),
            None => self.eval(extra, &Row::default()),
        }
    }

    fn compute(&self, aggregate: &Aggregate, rows: &[Row]) -> Result<Datum, Error> {
        let Some(argument) = &aggregate.argument else {
            return Ok(Datum::Integer(
                i64::try_from(rows.len()).unwrap_or(i64::MAX),
            ));
        };
        let mut values = Vec::new();
        let mut seen = BTreeSet::new();
        for row in rows {
            let value = self.eval(argument, row)?;
            if value.is_null() {
                continue;
            }
            if aggregate.distinct && !seen.insert(Key(vec![value.clone()])) {
                continue;
            }
            values.push(value);
        }
        match aggregate.function {
            AggregateFunction::Count => Ok(Datum::Integer(
                i64::try_from(values.len()).unwrap_or(i64::MAX),
            )),
            AggregateFunction::Collect => Ok(Datum::List(values)),
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
                    Some(extra) => self.group_percentile(extra, rows)?,
                    None => Datum::Null,
                };
                percentile_of(aggregate.function, values, &percentile)
            }
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
