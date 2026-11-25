use std::cell::Cell;
use std::num::NonZeroUsize;
use std::ptr;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::thread;

use crate::error::Error;
use crate::graph::GraphTx;
use crate::graph::NodeId;
use crate::graph::row_group;
use crate::result::QueryResult;
use crate::storage::ReadTransaction;
use crate::temporal::Clocks;

use super::aggregate::Groups;
use super::datum::Datum;
use super::eval::Env;
use super::eval::Row;
use super::exec;
use super::exec::counted_label;
use super::plan::Expr;
use super::plan::Items;
use super::plan::MatchClause;
use super::plan::PatternPart;
use super::plan::Program;
use super::plan::Projection;
use super::plan::ReadClause;
use super::regex::Patterns;
use super::scan::row_at_a_time;
use super::update::Graveyard;

const MORSELS_PER_THREAD: usize = 4;
const WORKER_STACK: usize = 2 << 20;
const DEFAULT_THRESHOLD: usize = 8192;

/// How many threads one read query may run on, and how many start nodes a match must read
/// before it splits them across those threads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Parallelism {
    pub(crate) threads: usize,
    pub(crate) threshold: usize,
}

impl Default for Parallelism {
    fn default() -> Self {
        Self {
            threads: thread::available_parallelism().map_or(1, NonZeroUsize::get),
            threshold: DEFAULT_THRESHOLD,
        }
    }
}

/// The threads running read queries of one database, its callers and their helpers, so a split
/// takes helpers only from the cores those threads leave idle.
#[derive(Debug, Default)]
pub(crate) struct Workers {
    busy: AtomicUsize,
}

/// Threads counted as busy in [`Workers`] until it drops.
#[derive(Debug)]
pub(crate) struct Busy<'w> {
    workers: &'w Workers,
    threads: usize,
}

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.workers.busy.fetch_sub(self.threads, Ordering::Relaxed);
    }
}

impl Workers {
    /// Counts the calling thread as busy while it runs a read query.
    pub(crate) fn enter(&self) -> Busy<'_> {
        self.busy.fetch_add(1, Ordering::Relaxed);
        Busy {
            workers: self,
            threads: 1,
        }
    }

    /// Takes up to `wanted` helper threads, as many as keep the busy threads within `threads`.
    pub(crate) fn reserve(&self, wanted: usize, threads: usize) -> Busy<'_> {
        let mut taken = 0;
        let _ = self
            .busy
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |busy| {
                taken = threads.saturating_sub(busy).min(wanted);
                (taken > 0).then(|| busy + taken)
            });
        Busy {
            workers: self,
            threads: taken,
        }
    }
}

/// The start nodes one worker of a parallel read matches its first pattern from: the run of
/// `ids` that `range` holds, which the worker moves to each morsel it takes.
#[derive(Debug)]
pub(crate) struct Morsel<'a> {
    part: &'a PatternPart,
    ids: &'a [NodeId],
    range: Cell<(usize, usize)>,
}

impl<'a, T: ReadTransaction> Env<'a, T> {
    /// The start nodes of `part` on a worker of a parallel read, when `part` is the pattern its
    /// morsel splits.
    pub(crate) fn morsel_of(&self, part: &PatternPart) -> Option<&'a [NodeId]> {
        let morsel = self.morsel.filter(|morsel| ptr::eq(morsel.part, part))?;
        let (start, end) = morsel.range.get();
        morsel.ids.get(start..end)
    }
}

/// Runs a read-only program, splitting a match that reads enough start nodes across
/// `parallelism.threads` threads, less those that other read queries of `workers` keep busy.
/// The rows, their order, and any error are those of [`exec::run_read`].
pub(crate) fn run_read<T: ReadTransaction>(
    tx: &GraphTx<T>,
    program: &Program<ReadClause>,
    parameters: &[Datum],
    now: &Clocks,
    parallelism: Parallelism,
    workers: &Workers,
) -> Result<QueryResult, Error>
where
    GraphTx<T>: Sync,
{
    if parallelism.threads < 2 || program.calls_host {
        return exec::run_read(tx, program, parameters, now);
    }
    let _running = workers.enter();
    exec::read_program(
        tx,
        program,
        parameters,
        now,
        |env, clause, next, rows| match split(env, parallelism, workers, clause, next, &rows) {
            Some(read) => match read.run(env, parallelism) {
                Some(done) => Ok(done),
                None => env.read_clauses(clause, next, rows),
            },
            None => env.read_clauses(clause, next, rows),
        },
    )
}

enum Job<'q> {
    Groups {
        found: &'q MatchClause,
        projection: &'q Projection,
    },
    Rows {
        next: Option<ReadClause>,
        projection: Option<&'q Projection>,
    },
}

enum Part<'q> {
    Groups(Groups<'q>),
    Rows(Vec<Row>, usize),
}

struct Split<'q, 'w> {
    clause: &'q ReadClause,
    part: &'q PatternPart,
    job: Job<'q>,
    row: &'q Row,
    ids: Vec<NodeId>,
    enough: Option<usize>,
    helpers: Busy<'w>,
}

fn split<'q, 'w, T: ReadTransaction>(
    env: &Env<'_, T>,
    parallelism: Parallelism,
    workers: &'w Workers,
    clause: &'q ReadClause,
    next: Option<&'q ReadClause>,
    rows: &'q [Row],
) -> Option<Split<'q, 'w>> {
    let ReadClause::Match(found) = clause else {
        return None;
    };
    let ([row], [part, ..]) = (rows, found.patterns.as_slice()) else {
        return None;
    };
    if found.optional
        || part.start.bound
        || env.check.is_some()
        || env.morsel.is_some()
        || row_at_a_time()
        || env.seek_possible(part).ok()?
    {
        return None;
    }
    let job = match next {
        Some(ReadClause::Project(projection)) => match &projection.items {
            Items::Aggregate {
                keys, aggregates, ..
            } => {
                if counted_label(found, keys, aggregates).is_some() {
                    return None;
                }
                Job::Groups { found, projection }
            }
            Items::Plain(_) if projection.distinct && projection.limit.is_none() => return None,
            Items::Plain(_) => Job::Rows {
                next: Some(ReadClause::Project(Box::new(per_morsel(env, projection)?))),
                projection: Some(projection),
            },
        },
        Some(ReadClause::CallSubquery(call)) if call.table => Job::Rows {
            next: None,
            projection: None,
        },
        _ => Job::Rows {
            next: next.cloned(),
            projection: None,
        },
    };
    let threshold = parallelism.threshold.max(2);
    if env.scanned_count(&part.start).ok()? < u64::try_from(threshold).ok()? {
        return None;
    }
    let helpers = workers.reserve(parallelism.threads - 1, parallelism.threads);
    if helpers.threads == 0 {
        return None;
    }
    let ids = env.scanned_nodes(&part.start).ok()?;
    let enough = match &job {
        Job::Rows {
            next: Some(ReadClause::Project(per_morsel)),
            projection: Some(projection),
        } if projection.order_by.is_empty() && !projection.distinct => {
            per_morsel.limit.as_ref().and_then(|limit| match limit {
                Expr::Constant(Datum::Integer(kept)) => usize::try_from(*kept).ok(),
                _ => None,
            })
        }
        _ => None,
    };
    Some(Split {
        clause,
        part,
        job,
        row,
        ids,
        enough,
        helpers,
    })
}

fn morsels(ids: &[NodeId], threads: usize) -> Vec<(usize, usize)> {
    let mut groups = Vec::new();
    let mut start = 0;
    while let Some(first) = ids.get(start) {
        let group = row_group(*first);
        let end = start + ids[start..].partition_point(|id| row_group(*id) == group);
        groups.push((start, end));
        start = end;
    }
    let wanted = threads.saturating_mul(MORSELS_PER_THREAD);
    let pieces = threads.div_ceil(groups.len().max(1));
    let size = ids.len().div_ceil(wanted).max(1);
    let mut morsels = Vec::new();
    for (start, end) in groups {
        if pieces > 1 {
            let piece = (end - start).div_ceil(pieces).max(1);
            morsels.extend(
                (start..end)
                    .step_by(piece)
                    .map(|at| (at, at.saturating_add(piece).min(end))),
            );
            continue;
        }
        match morsels.last_mut() {
            Some((first, last)) if *last == start && end - *first <= size => *last = end,
            _ => morsels.push((start, end)),
        }
    }
    morsels
}

fn prefix_reaching(counts: &[Option<usize>], enough: usize) -> Option<usize> {
    let mut total = 0usize;
    for (index, count) in counts.iter().enumerate() {
        total = total.saturating_add((*count)?);
        if total >= enough {
            return Some(index);
        }
    }
    None
}

fn per_morsel<T: ReadTransaction>(env: &Env<'_, T>, projection: &Projection) -> Option<Projection> {
    let empty = vec![Datum::Null; env.slots];
    let skip = match &projection.skip {
        Some(skip) => env.count(skip, &empty, "SKIP").ok()?,
        None => 0,
    };
    let limit = match &projection.limit {
        Some(limit) => Some(env.count(limit, &empty, "LIMIT").ok()?),
        None => None,
    };
    let kept = limit.and_then(|limit| i64::try_from(skip.saturating_add(limit)).ok());
    Some(Projection {
        skip: None,
        limit: kept.map(|kept| Expr::Constant(Datum::Integer(kept))),
        filter: None,
        ..projection.clone()
    })
}

impl<'q> Split<'q, '_> {
    fn run<T: ReadTransaction>(
        self,
        env: &Env<'_, T>,
        parallelism: Parallelism,
    ) -> Option<(Vec<Row>, usize)>
    where
        GraphTx<T>: Sync,
    {
        let morsels = morsels(&self.ids, parallelism.threads);
        let next = AtomicUsize::new(0);
        let failed = AtomicBool::new(false);
        let last = AtomicUsize::new(usize::MAX);
        let counts = Mutex::new(vec![None; morsels.len()]);
        let (tx, slots, parameters, now) = (env.tx, env.slots, env.parameters, env.now);
        let work = || {
            let graveyard = Graveyard::default();
            let patterns = Patterns::default();
            let morsel = Morsel {
                part: self.part,
                ids: &self.ids,
                range: Cell::new((0, 0)),
            };
            let worker = Env {
                morsel: Some(&morsel),
                ..Env::new(tx, slots, parameters, now, &graveyard, &patterns)
            };
            let mut done = Vec::new();
            while !failed.load(Ordering::Relaxed) {
                let index = next.fetch_add(1, Ordering::Relaxed);
                let Some(range) = morsels.get(index) else {
                    break;
                };
                if index > last.load(Ordering::Relaxed) {
                    break;
                }
                morsel.range.set(*range);
                let part = self.part_of(&worker);
                match &part {
                    Err(_) => failed.store(true, Ordering::Relaxed),
                    Ok(Part::Rows(rows, 2)) => {
                        if let Some(enough) = self.enough {
                            let mut counts = counts.lock().unwrap_or_else(PoisonError::into_inner);
                            if let Some(count) = counts.get_mut(index) {
                                *count = Some(rows.len());
                            }
                            if let Some(at) = prefix_reaching(&counts, enough) {
                                last.fetch_min(at, Ordering::Relaxed);
                            }
                        }
                    }
                    Ok(_) => {}
                }
                done.push((index, part));
            }
            done
        };
        let helpers = self.helpers.threads.min(morsels.len().saturating_sub(1));
        let finished = thread::scope(|scope| {
            let handles: Vec<_> = (0..helpers)
                .filter_map(|_| {
                    thread::Builder::new()
                        .stack_size(WORKER_STACK)
                        .spawn_scoped(scope, work)
                        .ok()
                })
                .collect();
            let mut finished = work();
            for handle in handles {
                match handle.join() {
                    Ok(done) => finished.extend(done),
                    Err(panic) => std::panic::resume_unwind(panic),
                }
            }
            finished
        });
        let kept = last.into_inner().saturating_add(1).min(morsels.len());
        let mut parts: Vec<Option<Part<'q>>> = (0..kept).map(|_| None).collect();
        for (index, part) in finished {
            if let Some(slot) = parts.get_mut(index) {
                *slot = Some(part.ok()?);
            }
        }
        self.combine(env, parts.into_iter().collect::<Option<Vec<_>>>()?)
            .ok()
            .flatten()
    }

    fn part_of<T: ReadTransaction>(&self, worker: &Env<'_, T>) -> Result<Part<'q>, Error> {
        let rows = std::slice::from_ref(self.row);
        match &self.job {
            Job::Groups { found, projection } => match &projection.items {
                Items::Aggregate {
                    keys, aggregates, ..
                } => Ok(Part::Groups(
                    worker.match_groups(found, keys, aggregates, rows)?,
                )),
                Items::Plain(_) => Ok(Part::Rows(Vec::new(), 0)),
            },
            Job::Rows { next, .. } => {
                let (rows, applied) =
                    worker.read_clauses(self.clause, next.as_ref(), rows.to_vec())?;
                Ok(Part::Rows(rows, applied))
            }
        }
    }

    fn combine<T: ReadTransaction>(
        &self,
        env: &Env<'_, T>,
        parts: Vec<Part<'q>>,
    ) -> Result<Option<(Vec<Row>, usize)>, Error> {
        match &self.job {
            Job::Groups { projection, .. } => {
                let Items::Aggregate {
                    keys,
                    aggregates,
                    finals,
                } = &projection.items
                else {
                    return Ok(None);
                };
                let mut groups = env.groups(keys, aggregates);
                for part in parts {
                    let Part::Groups(part) = part else {
                        return Ok(None);
                    };
                    groups.merge(part)?;
                }
                let rows = groups.finish(env, finals)?;
                Ok(Some((env.arrange(projection, rows)?, 2)))
            }
            Job::Rows { projection, .. } => {
                let mut rows = Vec::new();
                let mut applied = None;
                for part in parts {
                    let Part::Rows(part, part_applied) = part else {
                        return Ok(None);
                    };
                    if applied.is_some_and(|applied| applied != part_applied) {
                        return Ok(None);
                    }
                    rows.extend(part);
                    applied = Some(part_applied);
                }
                Ok(match (applied, projection) {
                    (Some(2), Some(projection)) => Some((env.arrange(projection, rows)?, 2)),
                    (Some(1), _) => Some((rows, 1)),
                    _ => None,
                })
            }
        }
    }
}

#[cfg(test)]
#[path = "parallel.test.rs"]
mod tests;
