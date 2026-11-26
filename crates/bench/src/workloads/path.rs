use crate::engine::EngineError;
use crate::engine::EngineKind;
use crate::engine::Query;
use crate::engine::Texts;
use crate::measure::Measurement;
use crate::workloads::Comparison;
use crate::workloads::Context;
use crate::workloads::Needs;
use crate::workloads::Workload;
use crate::workloads::run_queries;

const SHORTEST_PAIR: Texts = Texts {
    gql: Some(
        "MATCH p = ANY SHORTEST (a:Person {id: $from})-[:KNOWS]->+(b:Person {id: $to}) RETURN length(p)",
    ),
    cypher: Some(
        "MATCH (a:Person {id: $from}), (b:Person {id: $to}) MATCH p = shortestPath((a)-[:KNOWS*]->(b)) RETURN length(p)",
    ),
    sql: Some(
        "WITH RECURSIVE reach(node, depth) AS (SELECT dst, 1 FROM knows WHERE src = $from UNION SELECT k.dst, r.depth + 1 FROM reach r JOIN knows k ON k.src = r.node WHERE r.depth < 30) SELECT min(depth) FROM reach WHERE node = $to GROUP BY node",
    ),
    datalog: Some(
        "reach[b, min(d)] := *knows{src: $from, dst: b}, d = 1
         reach[b, min(d)] := reach[a, p], *knows{src: a, dst: b}, d = p + 1
         ?[d] := reach[$to, d]",
    ),
    overrides: &[(
        EngineKind::Ladybugdb,
        "MATCH p = (a:Person {id: $from})-[:KNOWS* SHORTEST 1..30]->(b:Person {id: $to}) RETURN length(p)",
    )],
};
const ALL_SHORTEST_PAIR: Texts = Texts {
    gql: Some(
        "MATCH p = ALL SHORTEST (a:Person {id: $from})-[:KNOWS]->+(b:Person {id: $to}) RETURN count(*), sum(length(p))",
    ),
    cypher: Some(
        "MATCH (a:Person {id: $from}), (b:Person {id: $to}) MATCH p = allShortestPaths((a)-[:KNOWS*]->(b)) RETURN count(*), sum(length(p))",
    ),
    sql: Some(
        "WITH RECURSIVE reach(node, depth) AS (SELECT $from, 0 UNION SELECT k.dst, r.depth + 1 FROM reach r JOIN knows k ON k.src = r.node WHERE r.depth < 30), distance AS (SELECT node, min(depth) AS d FROM reach GROUP BY node), walk(node, depth) AS (SELECT $from, 0 UNION ALL SELECT k.dst, w.depth + 1 FROM walk w JOIN knows k ON k.src = w.node JOIN distance ON distance.node = k.dst AND distance.d = w.depth + 1 WHERE w.depth < (SELECT d FROM distance WHERE node = $to)) SELECT count(*), sum(depth) FROM walk WHERE node = $to AND depth > 0",
    ),
    datalog: None,
    overrides: &[(
        EngineKind::Ladybugdb,
        "MATCH p = (a:Person {id: $from})-[:KNOWS* ALL SHORTEST 1..30]->(b:Person {id: $to}) RETURN count(*), sum(length(p))",
    )],
};
const SHORTEST_FROM_ONE: Texts = Texts {
    gql: Some(
        "MATCH p = ANY SHORTEST (a:Person {id: $id})-[:KNOWS]->+(b:Person) WHERE b.id <> $id RETURN count(*), sum(length(p))",
    ),
    cypher: None,
    sql: Some(
        "WITH RECURSIVE reach(node, depth) AS (SELECT dst, 1 FROM knows WHERE src = $id UNION SELECT k.dst, r.depth + 1 FROM reach r JOIN knows k ON k.src = r.node WHERE r.depth < 30), distance AS (SELECT node, min(depth) AS d FROM reach WHERE node <> $id GROUP BY node) SELECT count(*), sum(d) FROM distance",
    ),
    datalog: Some(
        "reach[b, min(d)] := *knows{src: $id, dst: b}, d = 1
         reach[b, min(d)] := reach[a, p], *knows{src: a, dst: b}, d = p + 1
         ?[count(b), sum(d)] := reach[b, d], b != $id",
    ),
    overrides: &[(
        EngineKind::Ladybugdb,
        "MATCH p = (a:Person {id: $id})-[:KNOWS* SHORTEST 1..30]->(b:Person) WHERE b.id <> $id RETURN count(*), sum(length(p))",
    )],
};
const QUANTIFIED: Texts = Texts {
    gql: Some(
        "MATCH (a:Person {id: $id})((x)-[k:KNOWS]->(y) WHERE k.since >= 2000){1,3}(b:Person) RETURN count(*), count(DISTINCT b)",
    ),
    cypher: Some(
        "MATCH (a:Person {id: $id})-[k:KNOWS*1..3]->(b:Person) WHERE all(r IN k WHERE r.since >= 2000) RETURN count(*), count(DISTINCT b)",
    ),
    sql: Some(
        "SELECT count(*), count(DISTINCT b) FROM (SELECT k1.dst AS b FROM knows k1 WHERE k1.src = $id AND k1.since >= 2000 UNION ALL SELECT k2.dst FROM knows k1 JOIN knows k2 ON k2.src = k1.dst WHERE k1.src = $id AND k1.since >= 2000 AND k2.since >= 2000 AND NOT (k2.src = k1.src AND k2.dst = k1.dst) UNION ALL SELECT k3.dst FROM knows k1 JOIN knows k2 ON k2.src = k1.dst JOIN knows k3 ON k3.src = k2.dst WHERE k1.src = $id AND k1.since >= 2000 AND k2.since >= 2000 AND k3.since >= 2000 AND NOT (k2.src = k1.src AND k2.dst = k1.dst) AND NOT (k3.src = k1.src AND k3.dst = k1.dst) AND NOT (k3.src = k2.src AND k3.dst = k2.dst)) AS trails",
    ),
    datalog: Some(
        "trail[b, a1, b1, a2, b2, a3, b3] := *knows{src: $id, dst: b, since}, since >= 2000, a1 = $id, b1 = b, a2 = -1, b2 = -1, a3 = -1, b3 = -1
         trail[b, a1, b1, a2, b2, a3, b3] := *knows{src: $id, dst: m, since: s1}, s1 >= 2000, *knows{src: m, dst: b, since: s2}, s2 >= 2000, a1 = $id, b1 = m, a2 = m, b2 = b, a3 = -2, b3 = -2, [a1, b1] != [a2, b2]
         trail[b, a1, b1, a2, b2, a3, b3] := *knows{src: $id, dst: m, since: s1}, s1 >= 2000, *knows{src: m, dst: n, since: s2}, s2 >= 2000, *knows{src: n, dst: b, since: s3}, s3 >= 2000, a1 = $id, b1 = m, a2 = m, b2 = n, a3 = n, b3 = b, [a1, b1] != [a2, b2], [a1, b1] != [a3, b3], [a2, b2] != [a3, b3]
         ?[count(b), count_unique(b)] := trail[b, a1, b1, a2, b2, a3, b3]",
    ),
    overrides: &[(
        EngineKind::Ladybugdb,
        "MATCH (a:Person {id: $id})-[:KNOWS* TRAIL 1..3 (r, _ | WHERE r.since >= 2000)]->(b:Person) RETURN count(*), count(DISTINCT b)",
    )],
};

pub fn workloads() -> Vec<Workload> {
    vec![
        path("path/shortest-pair", shortest_pair),
        path("path/all-shortest-pair", all_shortest_pair),
        path("path/shortest-from-one", shortest_from_one),
        path("path/quantified-1..3-filtered", quantified),
    ]
}

fn path(name: &'static str, run: fn(&Context<'_>) -> Result<Measurement, EngineError>) -> Workload {
    Workload {
        name,
        needs: Needs::Base,
        comparison: Comparison::Exact,
        file_only: false,
        run,
    }
}

fn pairs(ctx: &Context<'_>, texts: &Texts) -> Result<Measurement, EngineError> {
    let text = ctx.text(texts)?;
    let probes = &ctx.data.probes;
    let queries: Vec<Query<'static>> = probes
        .iter()
        .zip(probes.iter().cycle().skip(7))
        .filter(|(from, to)| from != to)
        .take(ctx.plan.traversals)
        .map(|(&from, &to)| Query::new(text).int("from", from).int("to", to))
        .collect();
    run_queries(ctx, &queries)
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

fn shortest_pair(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    pairs(ctx, &SHORTEST_PAIR)
}

fn all_shortest_pair(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    pairs(ctx, &ALL_SHORTEST_PAIR)
}

fn shortest_from_one(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    starting_at(ctx, &SHORTEST_FROM_ONE)
}

fn quantified(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    starting_at(ctx, &QUANTIFIED)
}
