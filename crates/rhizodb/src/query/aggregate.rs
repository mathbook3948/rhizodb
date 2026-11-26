use std::cmp::Ordering;
use std::collections::HashSet;

use crate::error::Error;
use crate::error::ErrorKind;
use crate::graph::NodeId;
use crate::graph::RelationshipId;
use crate::storage::ReadTransaction;

use super::compare::order;
use super::datum::Datum;
use super::datum::Limits;
use super::datum::ListBuilder;
use super::eval::Env;
use super::eval::Row;
use super::eval::set;
use super::eval::type_error;
use super::keys::KeyIndex;
use super::keys::KeySet;
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
    others: KeySet,
}

impl Seen {
    fn insert(&mut self, value: &Datum) -> bool {
        match value {
            Datum::Node(id) => self.nodes.insert(*id),
            Datum::Relationship(id) => self.relationships.insert(*id),
            other => self.others.insert(std::slice::from_ref(other)),
        }
    }
}

/// Which value of a row an aggregating projection asks for: a key, or the argument or the extra
/// argument of an aggregate, each by its index.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Part {
    Key(usize),
    Argument(usize),
    Extra(usize),
}

/// The groups an aggregating projection has seen so far, each with the state of every
/// aggregate. Rows are added one at a time, so a match can feed them without collecting rows.
pub(crate) struct Groups<'p> {
    keys: &'p [(Expr, Slot)],
    aggregates: &'p [(Aggregate, Slot)],
    index: KeyIndex,
    groups: Vec<(Vec<Datum>, Vec<Accumulator>)>,
    partial: bool,
    limits: Limits,
}

struct Accumulator {
    rows: usize,
    counted: usize,
    values: Vec<Datum>,
    best: Option<Datum>,
    list: ListBuilder,
    total: Total,
    seen: Seen,
    percentile: Option<Datum>,
}

struct Total {
    count: usize,
    integer: Option<i64>,
    floats: bool,
    float: f64,
}

impl Default for Total {
    fn default() -> Self {
        Self {
            count: 0,
            integer: Some(0),
            floats: false,
            float: -0.0,
        }
    }
}

impl Total {
    fn add(&mut self, value: &Datum) -> Result<(), Error> {
        let number = match value {
            Datum::Integer(value) => {
                self.integer = self.integer.and_then(|total| total.checked_add(*value));
                *value as f64
            }
            Datum::Float(value) => {
                self.floats = true;
                *value
            }
            other => {
                return Err(type_error(format!(
                    "expected a number, got a {}",
                    other.type_name()
                )));
            }
        };
        self.float += number;
        self.count += 1;
        Ok(())
    }

    fn sum(&self) -> Result<Datum, Error> {
        if self.floats {
            return Ok(Datum::Float(self.float));
        }
        self.integer
            .map(Datum::Integer)
            .ok_or_else(|| Error::new(ErrorKind::IntegerOverflow, "sum overflowed"))
    }

    fn average(&self) -> Datum {
        match self.count {
            0 => Datum::Null,
            count => Datum::Float(self.float / count as f64),
        }
    }
}

impl<'p> Groups<'p> {
    pub(crate) fn new(
        keys: &'p [(Expr, Slot)],
        aggregates: &'p [(Aggregate, Slot)],
        limits: Limits,
    ) -> Self {
        Self {
            keys,
            aggregates,
            index: KeyIndex::default(),
            groups: Vec::new(),
            partial: false,
            limits,
        }
    }

    /// Adds the groups and states of `other`, found by a worker of a parallel read, as if its
    /// rows came after every row added so far.
    pub(crate) fn merge(&mut self, other: Groups<'_>) -> Result<(), Error> {
        for (values, theirs) in other.groups {
            let position = self.group(values);
            if let Some((_, mine)) = self.groups.get_mut(position) {
                for ((aggregate, _), (mine, theirs)) in
                    self.aggregates.iter().zip(mine.iter_mut().zip(theirs))
                {
                    mine.merge(aggregate, theirs)?;
                }
            }
        }
        Ok(())
    }

    pub(crate) fn add<T: ReadTransaction>(
        &mut self,
        env: &Env<'_, T>,
        row: &Row,
    ) -> Result<(), Error> {
        self.add_with(|_, expr| env.eval(expr, row))
    }

    /// Adds one row whose values `eval` gives, asked in the order a row is evaluated.
    pub(crate) fn add_with(
        &mut self,
        mut eval: impl FnMut(Part, &Expr) -> Result<Datum, Error>,
    ) -> Result<(), Error> {
        let values = self
            .keys
            .iter()
            .enumerate()
            .map(|(index, (expr, _))| eval(Part::Key(index), expr))
            .collect::<Result<Vec<_>, _>>()?;
        let position = self.group(values);
        self.accumulate(position, eval)
    }

    /// The position of the group whose keys are `values`, added when there is none yet.
    pub(crate) fn group(&mut self, values: Vec<Datum>) -> usize {
        let found = if self.keys.is_empty() {
            (!self.groups.is_empty()).then_some(0)
        } else {
            self.index.get(&values)
        };
        match found {
            Some(position) => position,
            None => {
                let position = self.groups.len();
                let accumulators = self.accumulators();
                self.index.insert(values.clone(), position);
                self.groups.push((values, accumulators));
                position
            }
        }
    }

    fn accumulators(&self) -> Vec<Accumulator> {
        self.aggregates
            .iter()
            .map(|_| Accumulator::new(self.limits))
            .collect()
    }

    /// Adds one row to the group at `position`, which [`Groups::group`] gave, with the argument
    /// values `eval` gives.
    pub(crate) fn accumulate(
        &mut self,
        position: usize,
        mut eval: impl FnMut(Part, &Expr) -> Result<Datum, Error>,
    ) -> Result<(), Error> {
        if let Some((_, accumulators)) = self.groups.get_mut(position) {
            for (index, ((aggregate, _), accumulator)) in
                self.aggregates.iter().zip(accumulators).enumerate()
            {
                accumulator.add(aggregate, index, self.partial, &mut eval)?;
            }
        }
        Ok(())
    }

    /// Adds `rows` rows to the one group of a projection without keys whose aggregates all
    /// [count rows](Aggregate::counts_rows_binding) whose counted slot is never null, so they
    /// need not be read.
    pub(crate) fn count_rows(&mut self, rows: usize) {
        if self.groups.is_empty() {
            let accumulators = self.accumulators();
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
            let accumulators = self.accumulators();
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
    /// No groups yet, for the aggregating projection with `keys` and `aggregates`, partial on a
    /// worker of a parallel read.
    pub(crate) fn groups<'p>(
        &self,
        keys: &'p [(Expr, Slot)],
        aggregates: &'p [(Aggregate, Slot)],
    ) -> Groups<'p> {
        Groups {
            partial: self.morsel.is_some(),
            ..Groups::new(keys, aggregates, self.limits)
        }
    }

    /// Groups `rows` by the key values and yields one row per group with keys, aggregates, and
    /// final expressions filled in. Without keys, even no rows make one group.
    pub(crate) fn aggregate(
        &self,
        rows: Vec<Row>,
        keys: &[(Expr, Slot)],
        aggregates: &[(Aggregate, Slot)],
        finals: &[(Expr, Slot)],
    ) -> Result<Vec<Row>, Error> {
        let mut groups = Groups::new(keys, aggregates, self.limits);
        for row in &rows {
            groups.add(self, row)?;
        }
        groups.finish(self, finals)
    }
}

impl Accumulator {
    fn new(limits: Limits) -> Self {
        Self {
            rows: 0,
            counted: 0,
            values: Vec::new(),
            best: None,
            list: ListBuilder::new(limits),
            total: Total::default(),
            seen: Seen::default(),
            percentile: None,
        }
    }

    fn add(
        &mut self,
        aggregate: &Aggregate,
        index: usize,
        partial: bool,
        eval: &mut impl FnMut(Part, &Expr) -> Result<Datum, Error>,
    ) -> Result<(), Error> {
        self.rows += 1;
        let Some(argument) = &aggregate.argument else {
            return Ok(());
        };
        let function = aggregate.function;
        let value = eval(Part::Argument(index), argument)?;
        if !value.is_null() {
            function.admit(&value)?;
            match function {
                AggregateFunction::Min | AggregateFunction::Max => self.offer(aggregate, value),
                _ if aggregate.distinct && !self.seen.insert(&value) => {}
                AggregateFunction::Count if partial && aggregate.distinct => {
                    self.values.push(value);
                }
                AggregateFunction::Count => self.counted += 1,
                AggregateFunction::Sum | AggregateFunction::Avg if partial => {
                    self.values.push(value);
                }
                AggregateFunction::Sum | AggregateFunction::Avg => self.total.add(&value)?,
                AggregateFunction::Collect => self.list.push(value)?,
                _ => self.values.push(value),
            }
        }
        if let Some(extra) = &aggregate.extra
            && matches!(
                function,
                AggregateFunction::PercentileCont | AggregateFunction::PercentileDisc
            )
        {
            let value = eval(Part::Extra(index), extra)?;
            function.admit(&value)?;
            self.same_percentile(value)?;
        }
        Ok(())
    }

    fn same_percentile(&mut self, value: Datum) -> Result<(), Error> {
        match &self.percentile {
            Some(first) if order(first, &value) != Ordering::Equal => Err(Error::new(
                ErrorKind::InvalidArgumentValue,
                "the percentile must be the same for every row of a group",
            )),
            Some(_) => Ok(()),
            None => {
                self.percentile = Some(value);
                Ok(())
            }
        }
    }

    fn offer(&mut self, aggregate: &Aggregate, value: Datum) {
        let beats = |best: &Datum| match (aggregate.function, order(best, &value)) {
            (AggregateFunction::Min, Ordering::Greater) => true,
            (AggregateFunction::Max, Ordering::Less) => true,
            (AggregateFunction::Max, Ordering::Equal) => !aggregate.distinct,
            _ => false,
        };
        if self.best.as_ref().is_none_or(beats) {
            self.best = Some(value);
        }
    }

    fn merge(&mut self, aggregate: &Aggregate, other: Accumulator) -> Result<(), Error> {
        self.rows += other.rows;
        self.counted += other.counted;
        if let Some(percentile) = other.percentile {
            self.same_percentile(percentile)?;
        }
        if let Some(best) = other.best {
            self.offer(aggregate, best);
        }
        if aggregate.distinct {
            for value in other.list.into_items() {
                if self.seen.insert(&value) {
                    self.list.push(value)?;
                }
            }
        } else {
            self.list.append(other.list)?;
        }
        for value in other.values {
            if !aggregate.distinct || self.seen.insert(&value) {
                match aggregate.function {
                    AggregateFunction::Count => self.counted += 1,
                    AggregateFunction::Sum | AggregateFunction::Avg => self.total.add(&value)?,
                    _ => self.values.push(value),
                }
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
            AggregateFunction::Collect => Ok(self.list.finish()),
            AggregateFunction::Min | AggregateFunction::Max => Ok(self.best.unwrap_or(Datum::Null)),
            AggregateFunction::Sum => self.total.sum(),
            AggregateFunction::Avg => Ok(self.total.average()),
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
    let last = numbers.len() - 1;
    if function == AggregateFunction::PercentileDisc {
        let mut sorted = values;
        sorted.sort_by(order);
        let position = (percentile * numbers.len() as f64).ceil() as usize;
        let index = position.saturating_sub(1).min(last);
        return Ok(sorted.swap_remove(index));
    }
    numbers.sort_by(f64::total_cmp);
    let exact = percentile * last as f64;
    let lower = exact.floor() as usize;
    let upper = exact.ceil() as usize;
    let fraction = exact - lower as f64;
    Ok(Datum::Float(
        numbers[lower] + (numbers[upper.min(last)] - numbers[lower]) * fraction,
    ))
}
