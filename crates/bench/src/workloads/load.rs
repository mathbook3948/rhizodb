use crate::data::Facts;
use crate::data::Loaded;
use crate::engine::Dialect;
use crate::engine::EngineError;
use crate::engine::Instance;
use crate::engine::Param;
use crate::engine::Query;
use crate::engine::Texts;
use crate::fingerprint::Fingerprint;
use crate::fingerprint::RowHasher;
use crate::measure::Measurement;
use crate::measure::timed;
use crate::workloads::Comparison;
use crate::workloads::Context;
use crate::workloads::Needs;
use crate::workloads::Workload;

const BATCH: usize = 1_000;

const CREATE_PERSONS: Texts = Texts::graph(
    "UNWIND $rows AS r CREATE (:Person {id: r.id, name: r.name, age: r.age, bio: r.bio, embedding: r.embedding})",
);
const CREATE_CITIES: Texts =
    Texts::graph("UNWIND $rows AS r CREATE (:City {id: r.id, name: r.name})");
const CREATE_KNOWS: Texts = Texts::graph(
    "UNWIND $rows AS r MATCH (a:Person {id: r.src}), (b:Person {id: r.dst}) CREATE (a)-[:KNOWS {since: r.since}]->(b)",
);
const CREATE_LIVES_IN: Texts = Texts::graph(
    "UNWIND $rows AS r MATCH (a:Person {id: r.src}), (b:City {id: r.dst}) CREATE (a)-[:LIVES_IN]->(b)",
);
const CREATE_PERSON: Texts = Texts::graph(
    "CREATE (:Person {id: $id, name: $name, age: $age, bio: $bio, embedding: $embedding})",
);
const CREATE_PERSON_WITHOUT_AGE: Texts =
    Texts::graph("CREATE (:Person {id: $id, name: $name, bio: $bio, embedding: $embedding})");

pub(crate) const PERSON_STATS: Texts =
    Texts::graph("MATCH (p:Person) RETURN count(*), count(p.age), sum(p.age)");
const CITY_COUNT: Texts = Texts::graph("MATCH (c:City) RETURN count(*)");
const KNOWS_COUNT: Texts = Texts::graph("MATCH ()-[k:KNOWS]->() RETURN count(*)");
const LIVES_IN_COUNT: Texts = Texts::graph("MATCH ()-[l:LIVES_IN]->() RETURN count(*)");
const FIRST_QUERY: Texts = Texts::graph("MATCH (p:Person) RETURN count(*)");

pub fn workloads() -> Vec<Workload> {
    vec![
        workload("load/bulk", Needs::Empty, bulk),
        workload("load/statements", Needs::Empty, statements),
        workload("load/per-row-commit", Needs::Empty, per_row_commit),
        workload("reopen/open", Needs::ClosedCopy, open_only),
        workload("reopen/open+first-query", Needs::ClosedCopy, open_and_query),
    ]
}

fn workload(
    name: &'static str,
    needs: Needs,
    run: fn(&Context<'_>) -> Result<Measurement, EngineError>,
) -> Workload {
    Workload {
        name,
        needs,
        comparison: Comparison::Exact,
        run,
    }
}

pub(crate) fn read_state(
    session: &mut dyn crate::engine::Session,
    dialect: Dialect,
) -> Result<Fingerprint, EngineError> {
    let mut state = Fingerprint::default();
    for texts in [PERSON_STATS, CITY_COUNT, KNOWS_COUNT, LIVES_IN_COUNT] {
        let text = texts.for_dialect(dialect)?;
        state = state.merge(session.run(&Query::new(text))?);
    }
    Ok(state)
}

fn expected_state(facts: Facts) -> Fingerprint {
    let counts = [facts.cities, facts.knows, facts.lives_in];
    let mut expected = Fingerprint::default();
    let mut persons = RowHasher::default();
    persons.int(facts.persons as i64);
    persons.int(facts.aged as i64);
    persons.int(facts.age_sum);
    expected.absorb(persons);
    for count in counts {
        let mut row = RowHasher::default();
        row.int(count as i64);
        expected.absorb(row);
    }
    expected
}

fn run_load(
    ctx: &Context<'_>,
    loaded: Loaded,
    work: u64,
    prepare: impl FnOnce(&dyn Instance) -> Result<(), EngineError>,
    load: impl FnOnce(&dyn Instance) -> Result<(), EngineError>,
) -> Result<Measurement, EngineError> {
    let instance = ctx.engine.open(ctx.dir)?;
    prepare(&*instance)?;
    let (loading, result) = timed(|| {
        instance.create_schema()?;
        load(&*instance)
    });
    result?;
    let actual = read_state(&mut *instance.session()?, ctx.engine.dialect())?;
    let (closing, bytes) = timed(|| instance.finish(ctx.dir));
    let bytes = bytes?;
    let expected = expected_state(ctx.data.facts(loaded));
    let mismatch = actual != expected;
    Ok(Measurement {
        elapsed: loading + closing,
        ops: work,
        rows: work,
        errors: u64::from(mismatch),
        fingerprint: actual,
        bytes,
        failure: mismatch.then(|| "the loaded graph differs from the dataset".to_owned()),
    })
}

fn bulk(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    let loaded = ctx.data.loaded_in_full();
    let work = (loaded.persons + loaded.cities + loaded.knows + loaded.lives_in) as u64;
    run_load(
        ctx,
        loaded,
        work,
        |instance| instance.prepare_bulk_load(ctx.data, ctx.scratch),
        |instance| instance.bulk_load(ctx.data, ctx.scratch),
    )
}

fn statements(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    let data = ctx.data;
    let edges = ctx.plan.statement_edges;
    let loaded = Loaded {
        persons: data.people.len(),
        cities: data.cities.len(),
        knows: edges,
        lives_in: edges,
    };
    let work = (loaded.persons
        + loaded.cities
        + edges.min(data.knows.len())
        + edges.min(data.lives_in.len())) as u64;
    let create_persons = ctx.text(&CREATE_PERSONS)?;
    let create_cities = ctx.text(&CREATE_CITIES)?;
    let create_knows = ctx.text(&CREATE_KNOWS)?;
    let create_lives_in = ctx.text(&CREATE_LIVES_IN)?;
    run_load(
        ctx,
        loaded,
        work,
        |_| Ok(()),
        |instance| {
            let mut session = instance.session()?;
            session.begin()?;
            for batch in data.people.chunks(BATCH) {
                let rows = batch
                    .iter()
                    .map(|person| {
                        vec![
                            ("id", Param::Int(person.id)),
                            ("name", Param::Text(person.name.clone())),
                            ("age", person.age.map_or(Param::NullInt, Param::Int)),
                            ("bio", Param::Text(person.bio.clone())),
                            ("embedding", Param::Floats(person.embedding.clone())),
                        ]
                    })
                    .collect();
                session.run(&Query::new(create_persons).rows("rows", rows))?;
            }
            let cities = data
                .cities
                .iter()
                .map(|city| {
                    vec![
                        ("id", Param::Int(city.id)),
                        ("name", Param::Text(city.name.clone())),
                    ]
                })
                .collect();
            session.run(&Query::new(create_cities).rows("rows", cities))?;
            let knows = data
                .knows
                .iter()
                .take(edges)
                .map(|edge| {
                    vec![
                        ("src", Param::Int(edge.from)),
                        ("dst", Param::Int(edge.to)),
                        ("since", Param::Int(edge.since)),
                    ]
                })
                .collect();
            session.run(&Query::new(create_knows).rows("rows", knows))?;
            let lives_in = data
                .lives_in
                .iter()
                .take(edges)
                .map(|edge| {
                    vec![
                        ("src", Param::Int(edge.person)),
                        ("dst", Param::Int(edge.city)),
                    ]
                })
                .collect();
            session.run(&Query::new(create_lives_in).rows("rows", lives_in))?;
            session.commit()
        },
    )
}

fn per_row_commit(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    let data = ctx.data;
    let count = ctx.plan.per_row_loads.min(data.people.len());
    let loaded = Loaded {
        persons: count,
        cities: 0,
        knows: 0,
        lives_in: 0,
    };
    let create = ctx.text(&CREATE_PERSON)?;
    let create_without_age = ctx.text(&CREATE_PERSON_WITHOUT_AGE)?;
    run_load(
        ctx,
        loaded,
        count as u64,
        |_| Ok(()),
        |instance| {
            let mut session = instance.session()?;
            for person in data.people.iter().take(count) {
                let query = match person.age {
                    Some(age) => Query::new(create).int("age", age),
                    None => Query::new(create_without_age),
                };
                let query = query
                    .int("id", person.id)
                    .text("name", &person.name)
                    .text("bio", &person.bio)
                    .floats("embedding", &person.embedding);
                session.run(&query)?;
            }
            Ok(())
        },
    )
}

fn open_only(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    let (elapsed, instance) = timed(|| ctx.engine.open(ctx.dir));
    drop(instance?);
    Ok(Measurement {
        elapsed,
        ops: 1,
        ..Measurement::default()
    })
}

fn open_and_query(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    let first = ctx.text(&FIRST_QUERY)?;
    let (elapsed, result) = timed(|| {
        let instance = ctx.engine.open(ctx.dir)?;
        let fingerprint = instance.session()?.run(&Query::new(first))?;
        Ok::<_, EngineError>(fingerprint)
    });
    let fingerprint = result?;
    Ok(Measurement {
        elapsed,
        ops: 1,
        rows: fingerprint.rows,
        fingerprint,
        ..Measurement::default()
    })
}
