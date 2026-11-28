use crate::error::Error;
use crate::gql::ast::Direction;
use crate::gql::ast::LabelExpr;
use crate::graph::GraphError;
use crate::graph::NodeId;
use crate::graph::RelationshipBatch;
use crate::graph::SCANNED_RELATIONSHIP_KEYS;
use crate::graph::relationship_slots as slots;
use crate::storage::ReadTransaction;
use crate::storage::StorageError;

use super::aggregate::Folded;
use super::aggregate::Groups;
use super::eval::Env;
use super::eval::Row;
use super::plan::Aggregate;
use super::plan::AggregateFunction;
use super::plan::Expr;
use super::plan::MatchClause;
use super::plan::NodePattern;
use super::plan::Slot;
use super::scan::row_at_a_time;

const MAGNITUDE_NO_SUM_ORDER_OVERFLOWS: u128 = i64::MAX as u128;
const MAGNITUDE_EVERY_FLOAT_SUM_IS_EXACT: u128 = 1 << f64::MANTISSA_DIGITS;

/// `MATCH (a)-[r:T]->(b)`, one directed hop of fixed length whose ends have at most one label
/// and no property map, with no `WHERE`, fed into an aggregating projection without keys whose
/// aggregates count rows or read properties of `r`. The relationships are read chunk by chunk in
/// ID order rather than from each start node.
pub(crate) struct RelationshipScan<'q> {
    rel_type: Option<&'q str>,
    source: Option<&'q str>,
    target: Option<&'q str>,
    keys: Vec<&'q str>,
    folds: Vec<Fold>,
}

#[derive(Clone, Copy)]
enum Fold {
    Rows,
    Count(usize),
    Sum(usize, u128),
    Min(usize),
    Max(usize),
}

enum Halt {
    /// A value whose fold could depend on the order rows arrive in.
    Unordered,
    Failed(Error),
}

impl From<GraphError> for Halt {
    fn from(error: GraphError) -> Self {
        Self::Failed(error.into())
    }
}

impl From<StorageError> for Halt {
    fn from(error: StorageError) -> Self {
        Self::Failed(error.into())
    }
}

fn single_label(pattern: &NodePattern) -> Option<Option<&str>> {
    if pattern.bound || pattern.properties.is_some() {
        return None;
    }
    match &pattern.labels {
        None => Some(None),
        Some(LabelExpr::Name(name)) => Some(Some(name.0.as_str())),
        Some(_) => None,
    }
}

impl<'q> RelationshipScan<'q> {
    pub(crate) fn of(
        found: &'q MatchClause,
        keys: &[(Expr, Slot)],
        aggregates: &'q [(Aggregate, Slot)],
    ) -> Option<Self> {
        if found.optional
            || found.filter.is_some()
            || found.by_id.is_some()
            || !keys.is_empty()
            || row_at_a_time()
        {
            return None;
        }
        let [part] = found.patterns.as_slice() else {
            return None;
        };
        let [step] = part.steps.as_slice() else {
            return None;
        };
        let relationship = &step.relationship;
        if part.path.is_some()
            || relationship.bound
            || relationship.length.is_some()
            || relationship.properties.is_some()
            || part.start.slot == step.node.slot
        {
            return None;
        }
        let rel_type = match &relationship.labels {
            None => None,
            Some(LabelExpr::Name(name)) => Some(name.0.as_str()),
            Some(_) => return None,
        };
        let (start, far) = (single_label(&part.start)?, single_label(&step.node)?);
        let (source, target) = match relationship.direction {
            Direction::Outgoing | Direction::UndirectedOrOutgoing => (start, far),
            Direction::Incoming | Direction::IncomingOrUndirected => (far, start),
            _ => return None,
        };
        let bound = [part.start.slot, relationship.slot, step.node.slot];
        let mut keys: Vec<&str> = Vec::new();
        let mut folds = Vec::with_capacity(aggregates.len());
        for (aggregate, _) in aggregates {
            if bound
                .iter()
                .any(|slot| aggregate.counts_rows_binding(*slot))
            {
                folds.push(Fold::Rows);
                continue;
            }
            let Some(Expr::Property(base, key)) = &aggregate.argument else {
                return None;
            };
            if !matches!(base.as_ref(), Expr::Slot(slot) if *slot == relationship.slot)
                || aggregate.extra.is_some()
            {
                return None;
            }
            let at = match keys.iter().position(|held| held == key) {
                Some(at) => at,
                None if keys.len() < SCANNED_RELATIONSHIP_KEYS => {
                    keys.push(key);
                    keys.len() - 1
                }
                None => return None,
            };
            folds.push(match (aggregate.function, aggregate.distinct) {
                (AggregateFunction::Count, false) => Fold::Count(at),
                (AggregateFunction::Sum, false) => Fold::Sum(at, MAGNITUDE_NO_SUM_ORDER_OVERFLOWS),
                (AggregateFunction::Avg, false) => {
                    Fold::Sum(at, MAGNITUDE_EVERY_FLOAT_SUM_IS_EXACT)
                }
                (AggregateFunction::Min, _) => Fold::Min(at),
                (AggregateFunction::Max, _) => Fold::Max(at),
                _ => return None,
            });
        }
        Some(Self {
            rel_type,
            source,
            target,
            keys,
            folds,
        })
    }
}

struct Labeled(Vec<u64>);

impl Labeled {
    fn holds(&self, node: NodeId) -> bool {
        usize::try_from(node.0 / 64)
            .ok()
            .and_then(|word| self.0.get(word))
            .is_some_and(|word| word & (1 << (node.0 % 64)) != 0)
    }
}

impl<T: ReadTransaction> Env<'_, T> {
    /// The groups of an aggregating projection over a [`RelationshipScan`] fed by one row, or
    /// `None` when the clause has another form or a value it meets could make the result depend
    /// on the order of the rows, which only the path that matches from each start node fixes.
    pub(crate) fn relationship_groups<'p>(
        &self,
        found: &MatchClause,
        keys: &'p [(Expr, Slot)],
        aggregates: &'p [(Aggregate, Slot)],
        rows: &[Row],
    ) -> Result<Option<Groups<'p>>, Error> {
        let Some(scan) = RelationshipScan::of(found, keys, aggregates) else {
            return Ok(None);
        };
        if rows.len() != 1 || self.check.is_some() || self.morsel.is_some() {
            return Ok(None);
        }
        let mut folded = vec![Folded::default(); scan.folds.len()];
        let mut groups = self.groups(keys, aggregates);
        let rel_type = match scan.rel_type {
            Some(name) => match self.rel_type_token(name)? {
                Some(token) => Some(token),
                None => {
                    groups.fold(folded);
                    return Ok(Some(groups));
                }
            },
            None => None,
        };
        let Some(source) = self.labeled(scan.source)? else {
            groups.fold(folded);
            return Ok(Some(groups));
        };
        let other = match scan.target == scan.source {
            true => None,
            false => match self.labeled(scan.target)? {
                Some(target) => Some(target),
                None => {
                    groups.fold(folded);
                    return Ok(Some(groups));
                }
            },
        };
        let target = other.as_ref().unwrap_or(&source);
        let tokens = scan
            .keys
            .iter()
            .map(|key| self.property_key(key))
            .collect::<Result<Vec<_>, _>>()?;
        let ends = source.is_some() || target.is_some();
        let mut rows = 0usize;
        let mut magnitudes = vec![0u128; scan.folds.len()];
        let scanned = self
            .tx
            .scan_relationships(rel_type, ends, &tokens, |batch| {
                let mut kept = batch.present;
                if ends {
                    for slot in slots(kept) {
                        let fits = |nodes: &Option<Labeled>, node| {
                            nodes.as_ref().is_none_or(|nodes| nodes.holds(NodeId(node)))
                        };
                        if !fits(&source, batch.sources[slot]) || !fits(target, batch.targets[slot])
                        {
                            kept &= !(1 << slot);
                        }
                    }
                }
                rows += kept.count_ones() as usize;
                for ((fold, folded), magnitude) in
                    scan.folds.iter().zip(&mut folded).zip(&mut magnitudes)
                {
                    add(*fold, batch, kept, folded, magnitude)?;
                }
                Ok(())
            });
        match scanned {
            Ok(()) => {}
            Err(Halt::Unordered) => return Ok(None),
            Err(Halt::Failed(error)) => return Err(error),
        }
        for (fold, folded) in scan.folds.iter().zip(&mut folded) {
            folded.rows = rows;
            if let Fold::Rows = fold {
                folded.counted = rows;
            }
        }
        #[cfg(test)]
        tests::folded();
        groups.fold(folded);
        Ok(Some(groups))
    }

    fn labeled(&self, label: Option<&str>) -> Result<Option<Option<Labeled>>, Error> {
        let Some(label) = label else {
            return Ok(Some(None));
        };
        let Some(token) = self.label_token(label)? else {
            return Ok(None);
        };
        let mut words = Vec::new();
        for node in self.tx.node_ids_with_label(token)? {
            let word = usize::try_from(node.0 / 64).unwrap_or(usize::MAX);
            if words.len() <= word {
                words.resize(word + 1, 0);
            }
            words[word] |= 1 << (node.0 % 64);
        }
        Ok(Some(Some(Labeled(words))))
    }
}

fn add(
    fold: Fold,
    batch: &RelationshipBatch,
    kept: u32,
    folded: &mut Folded,
    magnitude: &mut u128,
) -> Result<(), Halt> {
    let at = match fold {
        Fold::Rows => return Ok(()),
        Fold::Count(at) => {
            folded.counted += (batch.columns[at].held & kept).count_ones() as usize;
            return Ok(());
        }
        Fold::Sum(at, _) | Fold::Min(at) | Fold::Max(at) => at,
    };
    let column = &batch.columns[at];
    let held = column.held & kept;
    if held & !column.integers != 0 {
        return Err(Halt::Unordered);
    }
    folded.counted += held.count_ones() as usize;
    let values = slots(held).map(|slot| column.values[slot]);
    match fold {
        Fold::Sum(_, bound) => {
            let (total, size) = values.fold((0i128, 0u128), |(total, size), value| {
                (
                    total + i128::from(value),
                    size + u128::from(value.unsigned_abs()),
                )
            });
            *magnitude += size;
            if *magnitude > bound {
                return Err(Halt::Unordered);
            }
            folded.total += total as i64;
        }
        Fold::Min(_) => folded.best = values.chain(folded.best).min(),
        Fold::Max(_) => folded.best = values.chain(folded.best).max(),
        Fold::Rows | Fold::Count(_) => {}
    }
    Ok(())
}

#[cfg(test)]
#[path = "relationship_scan.test.rs"]
mod tests;
