use std::sync::Barrier;
use std::thread;
use std::time::Instant;

use crate::engine::EngineError;
use crate::engine::Query;
use crate::engine::Texts;
use crate::measure::Measurement;
use crate::measure::Tally;
use crate::workloads::Comparison;
use crate::workloads::Context;
use crate::workloads::Needs;
use crate::workloads::Workload;
use crate::workloads::run_queries;

const POINT_LOOKUP: Texts = Texts::graph("MATCH (p:Person {id: $id}) RETURN p.name");
const COUNT_LABEL: Texts = Texts::graph("MATCH (p:Person) RETURN count(*)");
const FILTER_SCAN: Texts = Texts::graph("MATCH (p:Person) WHERE p.age > 30 RETURN count(*)");
const ORDER_LIMIT: Texts = Texts::graph(
    "MATCH (p:Person) WHERE p.age IS NOT NULL RETURN p.id, p.age ORDER BY p.age DESC, p.id ASC LIMIT 10",
);
const HOP_1: Texts = Texts::graph("MATCH (a:Person {id: $id})-[:KNOWS]->(b:Person) RETURN b.id");
const HOP_2: Texts = Texts::graph(
    "MATCH (a:Person {id: $id})-[:KNOWS]->(:Person)-[:KNOWS]->(c:Person) RETURN count(DISTINCT c)",
);
const HOP_3: Texts = Texts::graph(
    "MATCH (a:Person {id: $id})-[:KNOWS]->(:Person)-[:KNOWS]->(:Person)-[:KNOWS]->(d:Person) RETURN count(DISTINCT d)",
);
const VARIABLE_LENGTH: Texts =
    Texts::graph("MATCH (a:Person {id: $id})-[:KNOWS*1..3]->(b:Person) RETURN count(DISTINCT b)");
const GROUP_BY_CITY: Texts =
    Texts::graph("MATCH (p:Person)-[:LIVES_IN]->(c:City) RETURN c.name, count(p), avg(p.age)");
const DISTINCT_NAMES: Texts = Texts::graph("MATCH (p:Person) RETURN DISTINCT p.name");

pub fn workloads() -> Vec<Workload> {
    vec![
        read("read/point-lookup", point_lookup),
        read("read/count-label", count_label),
        read("read/filter-scan", filter_scan),
        read("read/order-limit", order_limit),
        read("traverse/1-hop", hop_1),
        read("traverse/2-hop-distinct", hop_2),
        read("traverse/3-hop-distinct", hop_3),
        read("traverse/variable-length-1..3", variable_length),
        read("aggregate/group-by-city", group_by_city),
        read("aggregate/distinct-names", distinct_names),
        read("concurrent/point-lookup-4-threads", concurrent_lookups),
    ]
}

fn read(name: &'static str, run: fn(&Context<'_>) -> Result<Measurement, EngineError>) -> Workload {
    Workload {
        name,
        needs: Needs::Base,
        comparison: Comparison::Exact,
        run,
    }
}

fn single(ctx: &Context<'_>, texts: &Texts) -> Result<Measurement, EngineError> {
    run_queries(ctx, &[Query::new(ctx.text(texts)?)])
}

fn starting_at(ctx: &Context<'_>, texts: &Texts) -> Result<Measurement, EngineError> {
    let text = ctx.text(texts)?;
    let queries: Vec<Query<'static>> = ctx
        .data
        .probes
        .iter()
        .take(ctx.plan.traversals)
        .map(|&id| Query::new(text).int("id", id))
        .collect();
    run_queries(ctx, &queries)
}

fn point_lookup(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    let text = ctx.text(&POINT_LOOKUP)?;
    let queries: Vec<Query<'static>> = ctx
        .data
        .probes
        .iter()
        .take(ctx.plan.lookups)
        .map(|&id| Query::new(text).int("id", id))
        .collect();
    run_queries(ctx, &queries)
}

fn count_label(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    single(ctx, &COUNT_LABEL)
}

fn filter_scan(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    single(ctx, &FILTER_SCAN)
}

fn order_limit(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    single(ctx, &ORDER_LIMIT)
}

fn hop_1(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    starting_at(ctx, &HOP_1)
}

fn hop_2(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    starting_at(ctx, &HOP_2)
}

fn hop_3(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    starting_at(ctx, &HOP_3)
}

fn variable_length(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    starting_at(ctx, &VARIABLE_LENGTH)
}

fn group_by_city(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    single(ctx, &GROUP_BY_CITY)
}

fn distinct_names(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    single(ctx, &DISTINCT_NAMES)
}

const THREADS: usize = 4;

fn concurrent_lookups(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    let instance = ctx.instance()?;
    let text = ctx.text(&POINT_LOOKUP)?;
    let lookups = ctx.plan.lookups;
    let probes = &ctx.data.probes;
    let barrier = Barrier::new(THREADS + 1);
    let (elapsed, tallies) = thread::scope(|scope| {
        let workers: Vec<_> = (0..THREADS)
            .map(|worker| {
                let barrier = &barrier;
                scope.spawn(move || {
                    let mut tally = Tally::default();
                    let session = instance.session().and_then(|mut session| {
                        session.prepare(text)?;
                        Ok(session)
                    });
                    barrier.wait();
                    match session {
                        Ok(mut session) => {
                            for step in 0..lookups {
                                let index = (worker * lookups + step) % probes.len().max(1);
                                if let Some(&id) = probes.get(index) {
                                    tally.record(session.run(&Query::new(text).int("id", id)));
                                }
                            }
                        }
                        Err(error) => tally.record(Err(error)),
                    }
                    tally
                })
            })
            .collect();
        barrier.wait();
        let start = Instant::now();
        let tallies: Vec<Tally> = workers
            .into_iter()
            .map(|worker| {
                worker.join().unwrap_or_else(|_| Tally {
                    errors: 1,
                    ..Tally::default()
                })
            })
            .collect();
        (start.elapsed(), tallies)
    });
    let mut total = Tally::default();
    for tally in tallies {
        total.combine(tally);
    }
    Ok(total.finish(elapsed))
}
