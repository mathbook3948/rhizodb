use crate::engine::EngineError;
use crate::engine::EngineKind;
use crate::engine::Param;
use crate::engine::Query;
use crate::engine::Texts;
use crate::measure::Measurement;
use crate::measure::Tally;
use crate::measure::timed;
use crate::workloads::Comparison;
use crate::workloads::Context;
use crate::workloads::Needs;
use crate::workloads::Workload;
use crate::workloads::close;
use crate::workloads::load::CREATE_PERSON;
use crate::workloads::load::PERSON_STATS;
use crate::workloads::load::read_state;

const ROWS: usize = 1_000;
const SET_COMPUTED: Texts =
    Texts::graph("MATCH (p:Person) WHERE p.age IS NOT NULL SET p.age = p.age + 1")
        .sql("UPDATE persons SET age = age + 1 WHERE age IS NOT NULL");
const SET_STRING: Texts =
    Texts::graph("MATCH (p:Person) WHERE p.id % 3 = 0 SET p.name = upper(p.name)")
        .sql("UPDATE persons SET name = upper(name) WHERE id % 3 = 0");
const REMOVE_PROPERTY: Texts = Texts::graph("MATCH (p:Person) WHERE p.id % 5 = 0 REMOVE p.age")
    .sql("UPDATE persons SET age = NULL WHERE id % 5 = 0")
    .overriding(&[(
        EngineKind::Ladybugdb,
        "MATCH (p:Person) WHERE p.id % 5 = 0 SET p.age = NULL",
    )]);
const SET_RELATIONSHIP: Texts = Texts::graph(
    "MATCH (a:Person)-[k:KNOWS]->(:Person) WHERE a.id % 7 = 0 SET k.since = k.since + 1",
)
.sql("UPDATE knows SET since = since + 1 WHERE src % 7 = 0");
const DELETE_RELATIONSHIPS: Texts =
    Texts::graph("MATCH (:Person)-[k:KNOWS]->(:Person) WHERE k.since < 1993 DELETE k")
        .sql("DELETE FROM knows WHERE since < 1993");
const MERGE_NODES: Texts = Texts::graph(
    "UNWIND $rows AS r MERGE (p:Person {id: r.id}) ON CREATE SET p.name = r.name, p.age = r.age ON MATCH SET p.age = r.age",
)
.sql(
    "INSERT INTO persons(id, name, age) VALUES ($id, $name, $age) ON CONFLICT(id) DO UPDATE SET age = excluded.age",
);
const MERGE_EDGES: Texts = Texts::graph(
    "UNWIND $rows AS r MATCH (a:Person {id: r.src}), (b:Person {id: r.dst}) MERGE (a)-[k:KNOWS]->(b) ON CREATE SET k.since = r.since",
)
.sql(
    "INSERT INTO knows(src, dst, since) SELECT $src, $dst, $since WHERE NOT EXISTS (SELECT 1 FROM knows WHERE src = $src AND dst = $dst)",
);
const CREATE_EDGES: Texts = Texts::graph(
    "UNWIND $rows AS r MATCH (a:Person {id: r.src}), (b:Person {id: r.dst}) CREATE (a)-[:KNOWS {since: r.since}]->(b)",
)
.sql("INSERT INTO knows(src, dst, since) VALUES ($src, $dst, $since)");
const CREATE_EDGES_FROM_PATTERN: Texts = Texts::graph(
    "MATCH (a:Person)-[:LIVES_IN]->(c:City {id: 1})<-[:LIVES_IN]-(b:Person) WHERE a.id < b.id CREATE (a)-[:KNOWS {since: 2025}]->(b)",
)
.sql(
    "INSERT INTO knows(src, dst, since) SELECT l1.src, l2.src, 2025 FROM lives_in l1 JOIN lives_in l2 ON l2.dst = l1.dst WHERE l1.dst = 1 AND l1.src < l2.src",
);
const SET_BY_ID: Texts = Texts::graph("MATCH (p:Person {id: $id}) SET p.age = $age")
    .sql("UPDATE persons SET age = $age WHERE id = $id");
const NAME_STATE: Texts =
    Texts::graph("MATCH (p:Person) RETURN count(DISTINCT p.name), min(p.name), max(p.name)")
        .sql("SELECT count(DISTINCT name), min(name), max(name) FROM persons");
const KNOWS_STATE: Texts = Texts::graph("MATCH ()-[k:KNOWS]->() RETURN count(*), sum(k.since)")
    .sql("SELECT count(*), sum(since) FROM knows");

const BULK_UPDATE: Texts = Texts::graph("MATCH (p:Person) WHERE p.id % 4 = 0 SET p.age = 99")
    .sql("UPDATE persons SET age = 99 WHERE id % 4 = 0")
    .datalog("?[id, age] := *persons{id}, mod(id, 4) == 0, age = 99 :update persons {id => age}");
const DETACH_DELETE: Texts =
    Texts::graph("MATCH (p:Person) WHERE p.id % 10 = 0 DETACH DELETE p")
        .sql(
            "BEGIN; DELETE FROM knows WHERE src % 10 = 0 OR dst % 10 = 0; DELETE FROM lives_in WHERE src % 10 = 0; DELETE FROM persons WHERE id % 10 = 0; COMMIT;",
        )
        .datalog(
            "{ ?[src, dst] := *knows{src, dst}, (mod(src, 10) == 0 or mod(dst, 10) == 0) :rm knows {src, dst} }
             { ?[src] := *lives_in{src}, mod(src, 10) == 0 :rm lives_in {src} }
             { ?[id] := *persons{id}, mod(id, 10) == 0 :rm persons {id} }",
        );

pub fn small_commits() -> Vec<Workload> {
    vec![closed("write/small-commits", small_commit_loop)]
}

pub fn bulk() -> Vec<Workload> {
    vec![
        closed("write/bulk-update", bulk_update),
        closed("write/detach-delete", detach_delete),
        closed("write/set-computed", set_computed),
        closed("write/set-string", set_string),
        closed("write/remove-property", remove_property),
        closed("write/set-relationship", set_relationship),
        closed("write/delete-relationships", delete_relationships),
        closed("write/merge-nodes", merge_nodes),
        closed("write/merge-edges", merge_edges),
        closed("write/create-edges-existing", create_edges),
        closed("write/create-edges-from-pattern", create_edges_from_pattern),
        closed("write/set-per-row-commit", set_per_row_commit),
    ]
}

fn closed(
    name: &'static str,
    run: fn(&Context<'_>) -> Result<Measurement, EngineError>,
) -> Workload {
    Workload {
        name,
        needs: Needs::ClosedCopy,
        comparison: Comparison::Exact,
        file_only: false,
        run,
    }
}

fn finish(
    ctx: &Context<'_>,
    instance: Box<dyn crate::engine::Instance>,
    mut measurement: Measurement,
) -> Result<Measurement, EngineError> {
    let (closing, bytes) = close(ctx, instance)?;
    measurement.elapsed += closing;
    measurement.close = closing;
    measurement.bytes = bytes;
    Ok(measurement)
}

fn small_commit_loop(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    let instance = ctx.engine.open(ctx.dir)?;
    let create = ctx.text(&CREATE_PERSON)?;
    let stats = ctx.text(&PERSON_STATS)?;
    let mut session = instance.session()?;
    session.prepare(create)?;
    let first_id = ctx.data.people.len() as i64 + 1;
    let commits = ctx.plan.small_commits;
    let mut tally = Tally::default();
    let (elapsed, ()) = timed(|| {
        for (offset, person) in ctx.data.people.iter().cycle().take(commits).enumerate() {
            let id = first_id + offset as i64;
            let query = Query::new(create)
                .int("id", id)
                .text("name", &person.name)
                .int("age", id % 60 + 18)
                .text("bio", &person.bio)
                .floats("embedding", &person.embedding);
            tally.record(session.run(&query));
        }
    });
    let state = session.run(&Query::new(stats))?;
    drop(session);
    let mut measurement = tally.finish(elapsed);
    measurement.rows = commits as u64;
    measurement.fingerprint = state;
    finish(ctx, instance, measurement)
}

fn mutate(ctx: &Context<'_>, texts: &Texts, rows: usize) -> Result<Measurement, EngineError> {
    mutate_with(ctx, texts, rows, |query| vec![query], &[])
}

fn mutate_with(
    ctx: &Context<'_>,
    texts: &Texts,
    rows: usize,
    queries: impl FnOnce(Query<'static>) -> Vec<Query<'static>>,
    states: &[Texts],
) -> Result<Measurement, EngineError> {
    let statement = ctx.text(texts)?;
    let queries = queries(Query::new(statement));
    let instance = ctx.engine.open(ctx.dir)?;
    let mut session = instance.session()?;
    session.prepare(statement)?;
    let mut tally = Tally::default();
    let (elapsed, ()) = timed(|| {
        for query in &queries {
            tally.record(session.run(query));
        }
    });
    let mut state = read_state(&mut *session, ctx.engine.kind(), ctx.engine.dialect())?;
    for (position, texts) in states.iter().enumerate() {
        let text = ctx.text(texts)?;
        state = state.merge_at(position as u64 + 4, session.run(&Query::new(text))?);
    }
    drop(session);
    let mut measurement = tally.finish(elapsed);
    measurement.rows = rows as u64;
    measurement.fingerprint = state;
    finish(ctx, instance, measurement)
}

fn bulk_update(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    let rows = ctx.data.people.iter().filter(|p| p.id % 4 == 0).count();
    mutate(ctx, &BULK_UPDATE, rows)
}

fn detach_delete(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    let rows = ctx.data.people.iter().filter(|p| p.id % 10 == 0).count();
    mutate(ctx, &DETACH_DELETE, rows)
}

fn set_computed(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    let rows = ctx.data.people.iter().filter(|p| p.age.is_some()).count();
    mutate(ctx, &SET_COMPUTED, rows)
}

fn set_string(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    let rows = ctx.data.people.iter().filter(|p| p.id % 3 == 0).count();
    mutate_with(ctx, &SET_STRING, rows, |query| vec![query], &[NAME_STATE])
}

fn remove_property(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    let rows = ctx.data.people.iter().filter(|p| p.id % 5 == 0).count();
    mutate(ctx, &REMOVE_PROPERTY, rows)
}

fn set_relationship(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    let rows = ctx.data.knows.iter().filter(|k| k.from % 7 == 0).count();
    mutate_with(
        ctx,
        &SET_RELATIONSHIP,
        rows,
        |query| vec![query],
        &[KNOWS_STATE],
    )
}

fn delete_relationships(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    let rows = ctx.data.knows.iter().filter(|k| k.since < 1993).count();
    mutate_with(
        ctx,
        &DELETE_RELATIONSHIPS,
        rows,
        |query| vec![query],
        &[KNOWS_STATE],
    )
}

fn persons(ctx: &Context<'_>) -> i64 {
    ctx.data.people.len() as i64
}

fn new_pairs(ctx: &Context<'_>) -> Vec<(i64, i64)> {
    let persons = persons(ctx);
    (0..ROWS as i64)
        .map(|step| {
            let from = step * 7919 % persons + 1;
            let to = (from + 1 + step % 13) % persons + 1;
            (from, if to == from { to % persons + 1 } else { to })
        })
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn edge_rows(pairs: impl Iterator<Item = (i64, i64)>) -> Vec<Vec<(&'static str, Param)>> {
    pairs
        .map(|(src, dst)| {
            vec![
                ("src", Param::Int(src)),
                ("dst", Param::Int(dst)),
                ("since", Param::Int(2000 + (src + dst) % 25)),
            ]
        })
        .collect()
}

fn merge_nodes(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    let first = persons(ctx) - ROWS as i64 / 2 + 1;
    let rows: Vec<Vec<(&'static str, Param)>> = (first..first + ROWS as i64)
        .map(|id| {
            vec![
                ("id", Param::Int(id)),
                ("name", Param::Text(format!("Merged {id}"))),
                ("age", Param::Int(id % 60 + 18)),
            ]
        })
        .collect();
    mutate_with(
        ctx,
        &MERGE_NODES,
        ROWS,
        |query| vec![query.rows("rows", rows)],
        &[NAME_STATE],
    )
}

fn merge_edges(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    let existing = ctx
        .data
        .knows
        .iter()
        .step_by(ctx.data.knows.len() / (ROWS / 2) + 1)
        .map(|edge| (edge.from, edge.to));
    let mut pairs: Vec<(i64, i64)> = existing
        .chain(new_pairs(ctx).into_iter().take(ROWS / 2))
        .collect();
    pairs.sort_unstable();
    pairs.dedup();
    let rows = edge_rows(pairs.into_iter());
    let count = rows.len();
    mutate_with(
        ctx,
        &MERGE_EDGES,
        count,
        |query| vec![query.rows("rows", rows)],
        &[KNOWS_STATE],
    )
}

fn create_edges(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    let rows = edge_rows(new_pairs(ctx).into_iter());
    let count = rows.len();
    mutate_with(
        ctx,
        &CREATE_EDGES,
        count,
        |query| vec![query.rows("rows", rows)],
        &[KNOWS_STATE],
    )
}

fn create_edges_from_pattern(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    let residents = ctx
        .data
        .lives_in
        .iter()
        .filter(|edge| edge.city == 1)
        .count();
    let rows = residents * residents.saturating_sub(1) / 2;
    mutate_with(
        ctx,
        &CREATE_EDGES_FROM_PATTERN,
        rows,
        |query| vec![query],
        &[KNOWS_STATE],
    )
}

fn set_per_row_commit(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    let commits = ctx.plan.small_commits;
    let probes = &ctx.data.probes;
    mutate_with(
        ctx,
        &SET_BY_ID,
        commits,
        |query| {
            (0..commits)
                .map(|step| {
                    let id = probes.get(step % probes.len().max(1)).copied().unwrap_or(1);
                    query
                        .clone()
                        .int("id", id)
                        .int("age", 18 + step as i64 % 60)
                })
                .collect()
        },
        &[],
    )
}
