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
use crate::workloads::load::PERSON_STATS;
use crate::workloads::load::read_state;

const CREATE_PERSON: Texts = Texts::graph(
    "CREATE (:Person {id: $id, name: $name, age: $age, bio: $bio, embedding: $embedding})",
);
const BULK_UPDATE: Texts = Texts::graph("MATCH (p:Person) WHERE p.id % 4 = 0 SET p.age = 99");
const DETACH_DELETE: Texts = Texts::graph("MATCH (p:Person) WHERE p.id % 10 = 0 DETACH DELETE p");

pub fn small_commits() -> Vec<Workload> {
    vec![copy("write/small-commits", small_commit_loop)]
}

pub fn bulk() -> Vec<Workload> {
    vec![
        copy("write/bulk-update", bulk_update),
        copy("write/detach-delete", detach_delete),
    ]
}

fn copy(name: &'static str, run: fn(&Context<'_>) -> Result<Measurement, EngineError>) -> Workload {
    Workload {
        name,
        needs: Needs::Copy,
        comparison: Comparison::Exact,
        run,
    }
}

fn small_commit_loop(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    let instance = ctx.instance()?;
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
    let mut measurement = tally.finish(elapsed);
    measurement.rows = commits as u64;
    measurement.fingerprint = state;
    Ok(measurement)
}

fn mutate(ctx: &Context<'_>, texts: &Texts, rows: usize) -> Result<Measurement, EngineError> {
    let statement = ctx.text(texts)?;
    let instance = ctx.instance()?;
    let mut session = instance.session()?;
    session.prepare(statement)?;
    let mut tally = Tally::default();
    let (elapsed, ()) = timed(|| tally.record(session.run(&Query::new(statement))));
    let state = read_state(&mut *session, ctx.engine.dialect())?;
    let mut measurement = tally.finish(elapsed);
    measurement.rows = rows as u64;
    measurement.fingerprint = state;
    Ok(measurement)
}

fn bulk_update(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    let rows = ctx.data.people.iter().filter(|p| p.id % 4 == 0).count();
    mutate(ctx, &BULK_UPDATE, rows)
}

fn detach_delete(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    let rows = ctx.data.people.iter().filter(|p| p.id % 10 == 0).count();
    mutate(ctx, &DETACH_DELETE, rows)
}
