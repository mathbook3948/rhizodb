use std::cell::Cell;
use std::cell::Ref;
use std::cell::RefCell;
use std::num::NonZeroUsize;
use std::ops::Deref;
use std::ops::Range;
use std::ptr;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::PoisonError;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::thread;

use rayon_core::ThreadPool;
use rayon_core::ThreadPoolBuilder;

use crate::error::Error;
use crate::graph::GraphTx;
use crate::graph::Label;
use crate::graph::NodeId;
use crate::graph::Token;
use crate::graph::row_group;
use crate::result::QueryResult;
use crate::storage::ReadTransaction;
use crate::temporal::Clocks;

use super::aggregate::Groups;
use super::datum::Datum;
use super::datum::Limits;
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
/// takes helpers only from the cores those threads leave idle, and the pool its helpers run on.
#[derive(Debug, Default)]
pub(crate) struct Workers {
    busy: AtomicUsize,
    pool: OnceLock<Option<ThreadPool>>,
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

    /// The `helpers` threads that run the helpers of every split, started by the first split and
    /// kept until the database drops, or `None` when they could not be started.
    pub(crate) fn pool(&self, helpers: usize) -> Option<&ThreadPool> {
        self.pool
            .get_or_init(|| {
                ThreadPoolBuilder::new()
                    .num_threads(helpers)
                    .stack_size(WORKER_STACK)
                    .thread_name(|index| format!("rhizodb-read-{index}"))
                    .build()
                    .ok()
            })
            .as_ref()
    }
}

/// The start nodes one worker of a parallel read matches its first pattern from: the run that
/// `range` holds of the `listed` start nodes, or else of the row group in `group`, which the
/// worker moves to each morsel it takes.
#[derive(Debug)]
pub(crate) struct Morsel<'a> {
    part: &'a PatternPart,
    listed: Option<&'a [NodeId]>,
    group: RefCell<Vec<NodeId>>,
    range: Cell<(usize, usize)>,
    /// Set when the clause on the morsel stops at its `LIMIT` because no row past it can fail,
    /// so the rows of a morsel after it are never needed once enough rows are found.
    pub(crate) stops: Cell<bool>,
}

/// The start nodes of a worker's morsel, borrowed from the listed start nodes or from the row
/// group the worker read.
pub(crate) enum MorselNodes<'a> {
    Listed(&'a [NodeId]),
    Group(Ref<'a, [NodeId]>),
}

impl Deref for MorselNodes<'_> {
    type Target = [NodeId];

    fn deref(&self) -> &[NodeId] {
        match self {
            Self::Listed(ids) => ids,
            Self::Group(ids) => ids,
        }
    }
}

impl<'a, T: ReadTransaction> Env<'a, T> {
    /// The start nodes of `part` on a worker of a parallel read, when `part` is the pattern its
    /// morsel splits.
    pub(crate) fn morsel_of(&self, part: &PatternPart) -> Option<MorselNodes<'a>> {
        let morsel = self.morsel.filter(|morsel| ptr::eq(morsel.part, part))?;
        let (start, end) = morsel.range.get();
        Some(match morsel.listed {
            Some(listed) => MorselNodes::Listed(&listed[start..end]),
            None => MorselNodes::Group(Ref::map(morsel.group.borrow(), |group| &group[start..end])),
        })
    }

    /// Notes that the clause on this worker's morsel stops at its `LIMIT` once it has its rows.
    pub(crate) fn stopping_at_limit(&self) {
        if let Some(morsel) = self.morsel {
            morsel.stops.set(true);
        }
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
    limits: Limits,
    parallelism: Parallelism,
    workers: &Workers,
) -> Result<QueryResult, Error>
where
    GraphTx<T>: Sync,
{
    if parallelism.threads < 2 || program.calls_host {
        return exec::run_read(tx, program, parameters, now, limits);
    }
    let _running = workers.enter();
    exec::read_program(
        tx,
        program,
        parameters,
        now,
        limits,
        |env, clauses, rows| {
            env.read_run(clauses, rows, |clauses, streamed, rows| {
                if let Some(done) = env.relationship_clauses(clauses, &rows)? {
                    return Ok(done);
                }
                match split(env, parallelism, workers, clauses, streamed, &rows) {
                    Some(read) => match read.run(env) {
                        Some(done) => Ok(done),
                        None => env.read_clauses_at(clauses, streamed, rows),
                    },
                    None => env.read_clauses_at(clauses, streamed, rows),
                }
            })
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

enum Source {
    Listed {
        ids: Vec<NodeId>,
        morsels: Vec<(usize, usize)>,
    },
    RowGroups(Option<Token<Label>>),
}

enum Claim {
    Nodes(usize, usize),
    EmptyUntil(usize),
    NoneLeft,
}

impl Source {
    fn claim<T: ReadTransaction>(
        &self,
        tx: &GraphTx<T>,
        index: usize,
        group: &mut Vec<NodeId>,
    ) -> Result<Claim, Error> {
        match self {
            Source::Listed { morsels, .. } => Ok(match morsels.get(index) {
                Some(&(start, end)) => Claim::Nodes(start, end),
                None => Claim::NoneLeft,
            }),
            Source::RowGroups(label) => {
                let from = index as u64;
                let mut claim = Claim::NoneLeft;
                tx.visit_node_groups(from, *label, |at, found| {
                    claim = if at == from {
                        group.clear();
                        group.extend_from_slice(found);
                        Claim::Nodes(0, found.len())
                    } else {
                        Claim::EmptyUntil(usize::try_from(at).unwrap_or(index + 1))
                    };
                    Ok::<_, Error>(false)
                })?;
                Ok(claim)
            }
        }
    }
}

struct Split<'q, 'w> {
    clause: &'q ReadClause,
    part: &'q PatternPart,
    job: Job<'q>,
    row: &'q Row,
    source: Source,
    alone: usize,
    enough: Option<usize>,
    helpers: Busy<'w>,
    pool: &'w ThreadPool,
}

fn split<'q, 'w, T: ReadTransaction>(
    env: &Env<'_, T>,
    parallelism: Parallelism,
    workers: &'w Workers,
    clauses: &[&'q ReadClause],
    streamed: Option<usize>,
    rows: &'q [Row],
) -> Option<Split<'q, 'w>> {
    let (&clause, next) = (clauses.first()?, clauses.get(1).copied());
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
    let stops = env.stops_early(clauses, streamed);
    let threshold = parallelism.threshold.max(2);
    let past = NodeId(u64::try_from(threshold).ok()?);
    if (stops && (enough.is_none() || !env.tx.may_hold_from(past).ok()?))
        || (!stops && env.scanned_count(&part.start).ok()? < u64::try_from(threshold).ok()?)
        || !ptr::eq(env.oriented(part, row).ok()?.0, part)
    {
        return None;
    }
    let helpers = workers.reserve(parallelism.threads - 1, parallelism.threads);
    if helpers.threads == 0 {
        return None;
    }
    let pool = workers.pool(parallelism.threads - 1)?;
    let (source, alone) = if stops {
        (
            Source::RowGroups(env.scanned_label(&part.start).ok()??),
            threshold,
        )
    } else {
        let ids = env.scanned_nodes(&part.start).ok()?;
        let morsels = morsels(&ids, parallelism.threads);
        (Source::Listed { ids, morsels }, 0)
    };
    Some(Split {
        clause,
        part,
        job,
        row,
        source,
        alone,
        enough,
        helpers,
        pool,
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
    let pieces = wanted.div_ceil(groups.len().max(1));
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

fn settle(
    counts: &Mutex<Vec<Option<usize>>>,
    range: Range<usize>,
    count: usize,
    enough: usize,
) -> Option<usize> {
    let mut counts = counts.lock().unwrap_or_else(PoisonError::into_inner);
    if counts.len() < range.end {
        counts.resize(range.end, None);
    }
    counts[range].fill(Some(count));
    prefix_reaching(&counts, enough)
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
    let (skip, limit) = env.window(projection).ok()?;
    let kept = limit.and_then(|limit| i64::try_from(skip.saturating_add(limit)).ok());
    Some(Projection {
        skip: None,
        limit: kept.map(|kept| Expr::Constant(Datum::Integer(kept))),
        filter: None,
        ..projection.clone()
    })
}

impl<'q> Split<'q, '_> {
    fn run<T: ReadTransaction>(self, env: &Env<'_, T>) -> Option<(Vec<Row>, usize)>
    where
        GraphTx<T>: Sync,
    {
        let next = AtomicUsize::new(0);
        let failed = AtomicBool::new(false);
        let last = AtomicUsize::new(usize::MAX);
        let counts = Mutex::new(Vec::new());
        let (tx, slots, parameters, now, limits, holds) = (
            env.tx,
            env.slots,
            env.parameters,
            env.now,
            env.limits,
            env.holds,
        );
        let work = |budget: usize| {
            let graveyard = Graveyard::default();
            let patterns = Patterns::default();
            let morsel = Morsel {
                part: self.part,
                listed: match &self.source {
                    Source::Listed { ids, .. } => Some(ids),
                    Source::RowGroups(_) => None,
                },
                group: RefCell::new(Vec::new()),
                range: Cell::new((0, 0)),
                stops: Cell::new(false),
            };
            let worker = Env {
                morsel: Some(&morsel),
                ..Env::new(tx, slots, parameters, now, &graveyard, &patterns, limits)
                    .with_holds(holds)
            };
            let mut done = Vec::new();
            let mut taken = 0;
            while taken < budget && !failed.load(Ordering::Relaxed) {
                let index = next.fetch_add(1, Ordering::Relaxed);
                if index > last.load(Ordering::Relaxed) {
                    break;
                }
                let claim = self.source.claim(tx, index, &mut morsel.group.borrow_mut());
                let read = match claim {
                    Ok(Claim::Nodes(start, end)) => {
                        morsel.range.set((start, end));
                        end - start
                    }
                    Ok(Claim::EmptyUntil(start)) => {
                        next.fetch_max(start, Ordering::Relaxed);
                        if let Some(enough) = self.enough
                            && let Some(at) = settle(&counts, index..start, 0, enough)
                        {
                            last.fetch_min(at, Ordering::Relaxed);
                        }
                        done.extend((index..start).map(|empty| (empty, Ok(None))));
                        continue;
                    }
                    Ok(Claim::NoneLeft) => {
                        last.fetch_min(index, Ordering::Relaxed);
                        done.push((index, Ok(None)));
                        break;
                    }
                    Err(error) => {
                        failed.store(true, Ordering::Relaxed);
                        done.push((index, Err(error)));
                        break;
                    }
                };
                taken += read;
                morsel.stops.set(false);
                let part = self.part_of(&worker);
                match &part {
                    Err(_) => failed.store(true, Ordering::Relaxed),
                    Ok(Part::Rows(rows, 2)) if morsel.stops.get() => {
                        if let Some(enough) = self.enough
                            && let Some(at) = settle(&counts, index..index + 1, rows.len(), enough)
                        {
                            last.fetch_min(at, Ordering::Relaxed);
                        }
                    }
                    Ok(_) => {}
                }
                done.push((index, part.map(Some)));
            }
            done
        };
        let helpers = match &self.source {
            Source::Listed { morsels, .. } => {
                self.helpers.threads.min(morsels.len().saturating_sub(1))
            }
            Source::RowGroups(_) => self.helpers.threads,
        };
        let helped = Mutex::new(Vec::new());
        #[cfg_attr(
            not(test),
            expect(unused_variables, reason = "only tests read how many helpers ran")
        )]
        let (mut finished, spawned) = self.pool.in_place_scope(|scope| {
            let mut finished = work(self.alone);
            if failed.load(Ordering::Relaxed)
                || next.load(Ordering::Relaxed) > last.load(Ordering::Relaxed)
            {
                return (finished, 0);
            }
            let (work, helped) = (&work, &helped);
            for _ in 0..helpers {
                scope.spawn(move |_| {
                    let done = work(usize::MAX);
                    helped
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .extend(done);
                });
            }
            finished.extend(work(usize::MAX));
            (finished, helpers)
        });
        finished.extend(helped.into_inner().unwrap_or_else(PoisonError::into_inner));
        #[cfg(test)]
        tests::ran(spawned, &finished);
        let kept = last.into_inner().saturating_add(1);
        let mut parts: Vec<Option<Option<Part<'q>>>> = Vec::new();
        for (index, part) in finished {
            if index >= kept {
                continue;
            }
            if parts.len() <= index {
                parts.resize_with(index + 1, || None);
            }
            parts[index] = Some(part.ok()?);
        }
        let parts = parts.into_iter().collect::<Option<Vec<_>>>()?;
        self.combine(env, parts.into_iter().flatten().collect())
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
                let clauses: Vec<&ReadClause> = std::iter::once(self.clause).chain(next).collect();
                let (rows, applied) = worker.read_clauses(&clauses, rows.to_vec())?;
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
