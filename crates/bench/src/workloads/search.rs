use crate::engine::EngineError;
use crate::engine::Query;
use crate::engine::Texts;
use crate::measure::Measurement;
use crate::measure::timed;
use crate::workloads::Comparison;
use crate::workloads::Context;
use crate::workloads::Needs;
use crate::workloads::Workload;
use crate::workloads::run_queries;

const CONTAINS_TERM: Texts =
    Texts::graph("MATCH (p:Person) WHERE p.bio CONTAINS $term RETURN count(*)");

pub fn workloads() -> Vec<Workload> {
    vec![
        Workload {
            name: "index/keyword-create",
            needs: Needs::Copy,
            comparison: Comparison::RowsOnly,
            run: keyword_create,
        },
        Workload {
            name: "index/vector-create",
            needs: Needs::Copy,
            comparison: Comparison::RowsOnly,
            run: vector_create,
        },
        Workload {
            name: "index/keyword-search",
            needs: Needs::Indexed,
            comparison: Comparison::RowsOnly,
            run: keyword_search,
        },
        Workload {
            name: "index/vector-search",
            needs: Needs::Indexed,
            comparison: Comparison::RowsOnly,
            run: vector_search,
        },
        Workload {
            name: "scan/bio-contains-term",
            needs: Needs::Base,
            comparison: Comparison::Exact,
            run: contains_term,
        },
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

fn keyword_create(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    let instance = ctx.instance()?;
    let (elapsed, result) = timed(|| instance.create_keyword_index());
    result?;
    let probe = keyword_queries(ctx, instance.keyword_search());
    let checked = run_queries(ctx, &probe[..probe.len().min(1)])?;
    Ok(Measurement {
        elapsed,
        ops: 1,
        rows: ctx.data.people.len() as u64,
        errors: checked.errors,
        fingerprint: checked.fingerprint,
        failure: checked.failure,
        bytes: None,
    })
}

fn vector_create(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    let instance = ctx.instance()?;
    let (elapsed, result) = timed(|| instance.create_vector_index());
    result?;
    let probe = vector_queries(ctx, instance.vector_search());
    let checked = run_queries(ctx, &probe[..probe.len().min(1)])?;
    Ok(Measurement {
        elapsed,
        ops: 1,
        rows: ctx.data.people.len() as u64,
        errors: checked.errors,
        fingerprint: checked.fingerprint,
        failure: checked.failure,
        bytes: None,
    })
}

fn keyword_search(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    if let Some(indexes) = ctx.indexes {
        indexes.keyword.clone()?;
    }
    run_queries(ctx, &keyword_queries(ctx, ctx.instance()?.keyword_search()))
}

fn vector_search(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    if let Some(indexes) = ctx.indexes {
        indexes.vector.clone()?;
    }
    run_queries(ctx, &vector_queries(ctx, ctx.instance()?.vector_search()))
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
