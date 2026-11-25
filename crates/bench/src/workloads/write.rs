use crate::engine::EngineError;
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
    let statement = ctx.text(texts)?;
    let instance = ctx.engine.open(ctx.dir)?;
    let mut session = instance.session()?;
    session.prepare(statement)?;
    let mut tally = Tally::default();
    let (elapsed, ()) = timed(|| tally.record(session.run(&Query::new(statement))));
    let state = read_state(&mut *session, ctx.engine.kind(), ctx.engine.dialect())?;
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
