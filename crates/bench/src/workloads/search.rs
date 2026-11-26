use std::collections::BTreeSet;

use crate::data::SEARCH_LIMIT;
use crate::engine::Built;
use crate::engine::EngineError;
use crate::engine::EngineKind;
use crate::engine::Query;
use crate::engine::Session;
use crate::engine::Texts;
use crate::measure::Measurement;
use crate::measure::timed;
use crate::workloads::Comparison;
use crate::workloads::Context;
use crate::workloads::Needs;
use crate::workloads::Workload;
use crate::workloads::run_queries;

const CONTAINS_TERM: Texts =
    Texts::graph("MATCH (p:Person) WHERE p.bio CONTAINS $term RETURN count(*)")
        .sql("SELECT count(*) FROM persons WHERE instr(bio, $term) > 0")
        .datalog("?[count(id)] := *persons{id, bio}, str_includes(bio, $term)");

const KEYWORD_THEN_EXPAND: Texts = Texts {
    gql: Some(
        "CALL db.index.keyword.queryNodes('bio', $term, 10) YIELD node MATCH (node)-[:KNOWS]->(f:Person) RETURN node.id, count(f), max(f.age)",
    ),
    cypher: None,
    sql: Some(
        "SELECT p.id, count(*), max(f.age) FROM bio_fts JOIN persons p ON p.id = bio_fts.rowid JOIN knows k ON k.src = p.id JOIN persons f ON f.id = k.dst WHERE bio_fts MATCH $term GROUP BY p.id",
    ),
    datalog: None,
    overrides: &[(
        EngineKind::Ladybugdb,
        "CALL QUERY_FTS_INDEX('Person', 'bio_index', $term, top := 10) WITH node MATCH (node)-[:KNOWS]->(f:Person) RETURN node.id, count(f), max(f.age)",
    )],
};

const LOW_RECALL: f64 = 0.9;

pub fn workloads() -> Vec<Workload> {
    let workload = |name, needs, comparison, run| Workload {
        name,
        needs,
        comparison,
        file_only: false,
        run,
    };
    vec![
        workload(
            "index/keyword-create",
            Needs::Copy,
            Comparison::GroundTruth,
            keyword_create,
        ),
        workload(
            "index/vector-create",
            Needs::Copy,
            Comparison::GroundTruth,
            vector_create,
        ),
        workload(
            "index/keyword-search",
            Needs::Indexed,
            Comparison::GroundTruth,
            keyword_search,
        ),
        workload(
            "index/vector-search",
            Needs::Indexed,
            Comparison::GroundTruth,
            vector_search,
        ),
        workload(
            "index/keyword-then-expand",
            Needs::Indexed,
            Comparison::Exact,
            keyword_then_expand,
        ),
        workload(
            "scan/bio-contains-term",
            Needs::Base,
            Comparison::Exact,
            contains_term,
        ),
    ]
}

fn keyword_queries(ctx: &Context<'_>, text: &'static str) -> Vec<Query<'static>> {
    (0..ctx.plan.searches)
        .filter_map(|index| ctx.data.terms.get(index % ctx.data.terms.len().max(1)))
        .map(|term| Query::new(text).text("term", term))
        .collect()
}

fn vector_queries(ctx: &Context<'_>, text: &'static str) -> Vec<Query<'static>> {
    (0..ctx.plan.searches)
        .filter_map(|index| {
            ctx.data
                .query_vectors
                .get(index % ctx.data.query_vectors.len().max(1))
        })
        .map(|vector| Query::new(text).floats("vector", vector))
        .collect()
}

fn create(
    ctx: &Context<'_>,
    build: fn(&dyn crate::engine::Instance) -> Result<Built, EngineError>,
    probe: fn(&Context<'_>) -> Result<Measurement, EngineError>,
) -> Result<Measurement, EngineError> {
    let instance = ctx.instance()?;
    instance.prepare_search()?;
    let (elapsed, built) = timed(|| build(instance));
    if built? == Built::Scan {
        return Err(EngineError::Unsupported(
            "the engine has no index to build, its search scans the data".to_owned(),
        ));
    }
    let checked = probe(ctx)?;
    Ok(Measurement {
        elapsed,
        ops: 1,
        rows: ctx.data.people.len() as u64,
        errors: checked.errors,
        fingerprint: checked.fingerprint,
        failure: checked.failure,
        ..Measurement::default()
    })
}

fn keyword_create(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    create(
        ctx,
        |instance| instance.create_keyword_index(),
        |ctx| {
            let probe = keyword_queries(ctx, ctx.instance()?.keyword_search()?);
            run_queries(ctx, &probe[..probe.len().min(1)])
        },
    )
}

fn vector_create(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    create(
        ctx,
        |instance| instance.create_vector_index(),
        |ctx| {
            let probe = vector_queries(ctx, ctx.instance()?.vector_search()?);
            run_queries(ctx, &probe[..probe.len().min(1)])
        },
    )
}

fn overlap(truth: &[i64], found: &[i64]) -> (f64, usize) {
    let wanted = truth.len().min(SEARCH_LIMIT);
    let truth: BTreeSet<i64> = truth.iter().copied().collect();
    let hits: BTreeSet<i64> = found
        .iter()
        .copied()
        .filter(|id| truth.contains(id))
        .collect();
    let recall = if wanted == 0 {
        1.0
    } else {
        hits.len().min(wanted) as f64 / wanted as f64
    };
    (recall, found.len() - hits.len())
}

fn judge(
    ctx: &Context<'_>,
    session: &mut dyn Session,
    queries: &[Query<'static>],
    truths: &[Vec<i64>],
    exact: bool,
    keyword: bool,
    measurement: &mut Measurement,
) -> Result<(), EngineError> {
    let mut total = 0.0;
    let mut stray = 0;
    for (index, query) in queries.iter().enumerate() {
        let found = session.ids(query)?;
        let truth = truths
            .get(index % truths.len().max(1))
            .map_or(&[][..], Vec::as_slice);
        let (recall, wrong) = overlap(truth, &found);
        total += recall;
        stray += wrong;
    }
    let recall = total / queries.len().max(1) as f64;
    measurement.recall = Some(recall);
    let kind = ctx.engine.kind().name();
    let false_hits = keyword && stray > 0;
    if exact && (recall < 1.0 || false_hits) {
        measurement.errors += 1;
        measurement.failure.get_or_insert_with(|| {
            format!(
                "{kind} is exact but recall is {recall:.3} with {stray} results outside the answer"
            )
        });
    } else if recall < LOW_RECALL {
        measurement.note = Some(format!("LOW RECALL {recall:.3}"));
    } else if false_hits {
        measurement.note = Some(format!("{stray} results outside the exact answer"));
    }
    Ok(())
}

fn keyword_search(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    let built = ctx.indexes.map(|indexes| indexes.keyword.clone());
    let built = built.transpose()?;
    let instance = ctx.instance()?;
    let queries = keyword_queries(ctx, instance.keyword_search()?);
    let mut measurement = run_queries(ctx, &queries)?;
    let mut session = instance.session()?;
    judge(
        ctx,
        &mut *session,
        &queries,
        &ctx.data.keyword_truth,
        ctx.engine.kind().exact_keyword_search(),
        true,
        &mut measurement,
    )?;
    if built == Some(Built::Scan) {
        measurement.note = Some(match measurement.note.take() {
            Some(note) => format!("scan, no index; {note}"),
            None => "scan, no index".to_owned(),
        });
    }
    Ok(measurement)
}

fn vector_search(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    let built = ctx.indexes.map(|indexes| indexes.vector.clone());
    let built = built.transpose()?;
    let instance = ctx.instance()?;
    let queries = vector_queries(ctx, instance.vector_search()?);
    let mut measurement = run_queries(ctx, &queries)?;
    let mut session = instance.session()?;
    judge(
        ctx,
        &mut *session,
        &queries,
        &ctx.data.vector_truth,
        ctx.engine.kind().exact_vector_search(),
        false,
        &mut measurement,
    )?;
    if built == Some(Built::Scan) {
        measurement.note = Some(match measurement.note.take() {
            Some(note) => format!("scan, no index; {note}"),
            None => "scan, no index".to_owned(),
        });
    }
    Ok(measurement)
}

fn contains_term(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    let text = ctx.text(&CONTAINS_TERM)?;
    let queries: Vec<Query<'static>> = ctx
        .data
        .terms
        .iter()
        .take(ctx.plan.traversals)
        .map(|term| Query::new(text).text("term", term))
        .collect();
    run_queries(ctx, &queries)
}

fn keyword_then_expand(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    if let Some(indexes) = ctx.indexes {
        indexes.keyword.clone()?;
    }
    let text = ctx.text(&KEYWORD_THEN_EXPAND)?;
    let rare: Vec<&String> = ctx
        .data
        .terms
        .iter()
        .zip(&ctx.data.keyword_truth)
        .filter(|(_, truth)| truth.len() < SEARCH_LIMIT)
        .map(|(term, _)| term)
        .collect();
    let queries: Vec<Query<'static>> = rare
        .iter()
        .cycle()
        .take(ctx.plan.searches)
        .map(|term| Query::new(text).text("term", term))
        .collect();
    run_queries(ctx, &queries)
}
