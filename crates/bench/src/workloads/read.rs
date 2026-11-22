use std::sync::Barrier;
use std::thread;
use std::time::Instant;

use crate::engine::EngineError;
use crate::engine::EngineKind;
use crate::engine::Query;
use crate::engine::Texts;
use crate::measure::Measurement;
use crate::measure::Tally;
use crate::workloads::Comparison;
use crate::workloads::Context;
use crate::workloads::Needs;
use crate::workloads::Workload;
use crate::workloads::load::COUNT_PERSONS;
use crate::workloads::run_queries;

const RETURN_ONE: Texts = Texts::graph("RETURN 1")
    .sql("SELECT 1")
    .datalog("?[x] <- [[1]]");
const POINT_LOOKUP: Texts = Texts::graph("MATCH (p:Person {id: $id}) RETURN p.name")
    .sql("SELECT name FROM persons WHERE id = $id")
    .datalog("?[name] := *persons{id: $id, name}");
const COUNT_LABEL: Texts = COUNT_PERSONS;
const FILTER_SCAN: Texts = Texts::graph("MATCH (p:Person) WHERE p.age > 30 RETURN count(*)")
    .sql("SELECT count(*) FROM persons WHERE age > 30")
    .datalog("?[count(id)] := *persons{id, age}, age != null, age > 30");
const ORDER_LIMIT: Texts = Texts::graph(
    "MATCH (p:Person) WHERE p.age IS NOT NULL RETURN p.id, p.age ORDER BY p.age DESC, p.id ASC LIMIT 10",
)
.sql("SELECT id, age FROM persons WHERE age IS NOT NULL ORDER BY age DESC, id ASC LIMIT 10")
.datalog("?[id, age] := *persons{id, age}, age != null :order -age, id :limit 10");
const HOP_1: Texts = Texts::graph("MATCH (a:Person {id: $id})-[:KNOWS]->(b:Person) RETURN b.id")
    .sql("SELECT dst FROM knows WHERE src = $id")
    .datalog("?[b] := *knows{src: $id, dst: b}");
const HOP_2: Texts = Texts::graph(
    "MATCH (a:Person {id: $id})-[:KNOWS]->(:Person)-[:KNOWS]->(c:Person) RETURN count(DISTINCT c)",
)
.sql("SELECT count(DISTINCT k2.dst) FROM knows k1 JOIN knows k2 ON k2.src = k1.dst WHERE k1.src = $id")
.datalog("?[count_unique(c)] := *knows{src: $id, dst: b}, *knows{src: b, dst: c}");
const DISTINCT_EDGES_3_HOP: &str = "MATCH (a:Person {id: $id})-[:KNOWS]->(b:Person)-[:KNOWS]->(c:Person)-[:KNOWS]->(d:Person) WHERE c.id <> a.id OR d.id <> b.id RETURN count(DISTINCT d)";
const HOP_3: Texts = Texts::graph(
    "MATCH (a:Person {id: $id})-[:KNOWS]->(:Person)-[:KNOWS]->(:Person)-[:KNOWS]->(d:Person) RETURN count(DISTINCT d)",
)
.sql(
    "SELECT count(DISTINCT k3.dst) FROM knows k1 JOIN knows k2 ON k2.src = k1.dst JOIN knows k3 ON k3.src = k2.dst WHERE k1.src = $id AND (k3.src <> k1.src OR k3.dst <> k1.dst)",
)
.datalog(
    "?[count_unique(d)] := *knows{src: $id, dst: b}, *knows{src: b, dst: c}, *knows{src: c, dst: d}, (c != $id or d != b)",
)
.overriding(&[
    (EngineKind::Ladybugdb, DISTINCT_EDGES_3_HOP),
    (EngineKind::Falkordb, DISTINCT_EDGES_3_HOP),
]);
const VARIABLE_LENGTH: Texts =
    Texts::graph("MATCH (a:Person {id: $id})-[:KNOWS*1..3]->(b:Person) RETURN count(DISTINCT b)")
        .sql(
            "WITH RECURSIVE reach(node, depth) AS (SELECT dst, 1 FROM knows WHERE src = $id UNION ALL SELECT k.dst, reach.depth + 1 FROM reach JOIN knows k ON k.src = reach.node WHERE reach.depth < 3) SELECT count(DISTINCT node) FROM reach",
        )
        .datalog(
            "reach[b, depth] := *knows{src: $id, dst: b}, depth = 1
             reach[b, depth] := reach[a, previous], previous < 3, *knows{src: a, dst: b}, depth = previous + 1
             ?[count_unique(b)] := reach[b, _]",
        );
const GROUP_BY_CITY: Texts =
    Texts::graph("MATCH (p:Person)-[:LIVES_IN]->(c:City) RETURN c.name, count(p), avg(p.age)")
        .sql(
            "SELECT c.name, count(p.id), avg(p.age) FROM persons p JOIN lives_in l ON l.src = p.id JOIN cities c ON c.id = l.dst GROUP BY c.id, c.name",
        )
        .datalog(
            "residents[name, count(person)] := *lives_in{src: person, dst: city}, *cities{id: city, name}
             ages[name, mean(age)] := *lives_in{src: person, dst: city}, *cities{id: city, name}, *persons{id: person, age}, age != null
             ?[name, residents, average] := residents[name, residents], ages[name, average]",
        );
const DISTINCT_NAMES: Texts = Texts::graph("MATCH (p:Person) RETURN DISTINCT p.name")
    .sql("SELECT DISTINCT name FROM persons")
    .datalog("?[name] := *persons{name}");

pub fn workloads() -> Vec<Workload> {
    vec![
        read("overhead/return-1", return_one),
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
        file_only: false,
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

fn return_one(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    let text = ctx.text(&RETURN_ONE)?;
    let queries: Vec<Query<'static>> = (0..ctx.plan.lookups).map(|_| Query::new(text)).collect();
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
                                    tally.record_at(
                                        (worker * lookups + step) as u64,
                                        session.run(&Query::new(text).int("id", id)),
                                    );
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
            .map(|worker| worker.join().unwrap_or_else(|_| Tally::one_error()))
            .collect();
        (start.elapsed(), tallies)
    });
    let mut total = Tally::default();
    for tally in tallies {
        total.combine(tally);
    }
    Ok(total.finish(elapsed))
}
