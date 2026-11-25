//! Random typed GQL over a random graph, checked two ways: against a reference evaluator that
//! computes each answer from a plain model of the graph, and across rewrites that must not
//! change the answer, such as moving a predicate into a `WITH`, `FILTER`, `EXISTS`, or
//! `OPTIONAL MATCH`, reversing a pattern, or adding property indexes. Queries also run inside a
//! write transaction that holds uncommitted edits, and on a file reopened from disk.

#[path = "support/query_oracle.rs"]
mod query_oracle;
mod support;

use std::cmp::Ordering;

use query_oracle::*;
use rhizodb::Database;
use rhizodb::IndexTarget;
use rhizodb::Value;
use support::Rng;
use support::cases;
use support::db;
use support::options;
use support::parallel_options;
use support::seed;

const FAST_GRAPHS: u64 = 24;
const ROUNDS_PER_GRAPH: u64 = 6;

impl Choices for Rng {
    fn below(&mut self, bound: u64) -> u64 {
        Rng::below(self, bound)
    }
}

type Rows = Vec<Vec<Value>>;

fn ints_sorted(rows: &Rows) -> Result<Vec<Vec<i64>>, String> {
    let mut out = Vec::new();
    for row in rows {
        let mut keys = Vec::new();
        for value in row {
            keys.push(match value {
                Value::Integer(value) => *value,
                Value::Null => i64::MIN,
                Value::List(items) => {
                    let mut packed = 0i64;
                    for item in items {
                        match item {
                            Value::Integer(item) => packed = packed * 1000 + item + 1,
                            other => return Err(format!("expected integers, got {other:?}")),
                        }
                    }
                    packed
                }
                other => return Err(format!("expected an integer, got {other:?}")),
            });
        }
        out.push(keys);
    }
    out.sort();
    Ok(out)
}

fn expect_bag(runner: &mut dyn Runner, query: &str, mut want: Vec<Vec<i64>>) -> Result<(), String> {
    let rows = runner.run(query)?;
    let got = ints_sorted(&rows)?;
    want.sort();
    if got != want {
        return Err(format!("{query}\n  got  {got:?}\n  want {want:?}"));
    }
    Ok(())
}

fn to_v(value: &Value) -> Result<V, String> {
    Ok(match value {
        Value::Null => V::Null,
        Value::Boolean(value) => V::Bool(*value),
        Value::Integer(value) => V::Int(*value),
        Value::Float(value) => V::Float(*value),
        Value::String(value) => V::Str(value.clone()),
        Value::List(items) => V::List(items.iter().map(to_v).collect::<Result<_, _>>()?),
        other => return Err(format!("unexpected value {other:?}")),
    })
}

fn node_scope() -> Scope {
    Scope {
        nodes: vec!["n"],
        rels: vec![],
        vars: 0,
    }
}

fn pattern_scope() -> Scope {
    Scope {
        nodes: vec!["a", "b"],
        rels: vec!["r"],
        vars: 0,
    }
}

fn holds(expression: &E, row: &Row<'_>) -> Option<bool> {
    match eval(expression, row) {
        V::Bool(value) => Some(value),
        _ => None,
    }
}

fn keys_where(graph: &Graph, p: &E, want: Option<bool>, label: Option<&str>) -> Vec<Vec<i64>> {
    graph
        .nodes
        .values()
        .filter(|node| label.is_none_or(|label| node.labels.contains(label)))
        .filter(|node| {
            holds(
                p,
                &Row {
                    nodes: vec![node],
                    rels: vec![],
                },
            ) == want
        })
        .map(|node| vec![node.k])
        .collect()
}

fn value_for(key: &str, choices: &mut Rng) -> V {
    match key {
        "x" => int_value(choices),
        "y" => str_value(choices),
        "z" => mixed_value(choices),
        _ => V::Bool(choices.chance(1, 2)),
    }
}

fn index_shaped(choices: &mut Rng) -> E {
    let key = *choices.pick(&["x", "y", "z", "t"]);
    let single = |choices: &mut Rng| {
        let op = *choices.pick(&["=", "<", "<=", ">", ">=", "IN"]);
        if op == "IN" {
            let items = (0..choices.below(4))
                .map(|_| value_for(key, choices))
                .collect();
            E::In(Box::new(E::NodeProp(0, key)), items)
        } else {
            E::Cmp(
                op,
                Box::new(E::NodeProp(0, key)),
                Box::new(E::Lit(value_for(key, choices))),
            )
        }
    };
    let first = single(choices);
    if choices.chance(1, 2) {
        E::Logic("AND", Box::new(first), Box::new(single(choices)))
    } else {
        first
    }
}

fn check_filter(runner: &mut dyn Runner, graph: &Graph, choices: &mut Rng) -> Result<(), String> {
    let scope = node_scope();
    let p = if choices.chance(1, 3) {
        index_shaped(choices)
    } else {
        expr(Ty::Bool, 3, &scope, choices)
    };
    let text = render(&p, &scope);
    let yes = keys_where(graph, &p, Some(true), None);
    let no = keys_where(graph, &p, Some(false), None);
    let unknown = keys_where(graph, &p, None, None);
    let with_m = render(
        &p,
        &Scope {
            nodes: vec!["m"],
            rels: vec![],
            vars: 0,
        },
    );
    let variants = [
        format!("MATCH (n) WHERE {text} RETURN n.k"),
        format!("MATCH (n) WITH n WHERE {text} RETURN n.k"),
        format!("MATCH (n) FILTER {text} RETURN n.k"),
        format!("MATCH (n) WITH n, {text} AS keep WHERE keep RETURN n.k"),
        format!("MATCH (n) WITH collect(n) AS ns UNWIND ns AS n WITH n WHERE {text} RETURN n.k"),
        format!("MATCH (n) WHERE EXISTS {{ MATCH (m) WHERE m = n AND {with_m} }} RETURN n.k"),
        format!("MATCH (m) WITH m AS n WHERE {text} RETURN n.k"),
        format!("MATCH (n) RETURN n NEXT FILTER {text} RETURN n.k"),
        format!("MATCH (n) WHERE VALUE {{ RETURN {text} }} RETURN n.k"),
        format!("MATCH (n) WHERE ({text}) IS TRUE RETURN n.k"),
        format!("MATCH (n) WHERE ({text}) IS NOT DISTINCT FROM true RETURN n.k"),
        format!("MATCH (n) WHERE NOT ({text}) IS NOT TRUE RETURN n.k"),
        format!("MATCH (n) RETURN n, {text} AS keep NEXT YIELD n, keep FILTER keep RETURN n.k"),
    ];
    let variant = choices.pick(&variants);
    expect_bag(runner, variant, yes.clone())?;
    let every: Vec<Vec<i64>> = graph.nodes.values().map(|node| vec![node.k]).collect();
    let not_yes: Vec<Vec<i64>> = every
        .iter()
        .filter(|row| !yes.contains(row))
        .cloned()
        .collect();
    let set_operations = [
        (
            format!("MATCH (n) RETURN n.k EXCEPT MATCH (n) WHERE {text} RETURN n.k"),
            not_yes.clone(),
        ),
        (
            format!("MATCH (n) RETURN n.k INTERSECT ALL MATCH (n) WHERE {text} RETURN n.k"),
            yes.clone(),
        ),
        (
            format!(
                "MATCH (n) RETURN n.k EXCEPT ALL \
                 MATCH (n) WHERE NOT coalesce({text}, false) RETURN n.k"
            ),
            yes.clone(),
        ),
        (
            format!("MATCH (n) WHERE {text} RETURN n.k OTHERWISE MATCH (n) RETURN n.k"),
            if yes.is_empty() {
                every.clone()
            } else {
                yes.clone()
            },
        ),
    ];
    let (query, want) = choices.pick(&set_operations);
    expect_bag(runner, query, want.clone())?;
    expect_bag(
        runner,
        &format!("MATCH (n) WHERE NOT {text} RETURN n.k"),
        no.clone(),
    )?;
    expect_bag(
        runner,
        &format!("MATCH (n) WHERE ({text}) IS NULL RETURN n.k"),
        unknown,
    )?;
    let label = *choices.pick(&LABELS);
    expect_bag(
        runner,
        &format!("MATCH (n:{label}) WHERE {text} RETURN n.k"),
        keys_where(graph, &p, Some(true), Some(label)),
    )?;
    let optional: Vec<Vec<i64>> = graph
        .nodes
        .values()
        .map(|node| {
            let kept = holds(
                &p,
                &Row {
                    nodes: vec![node],
                    rels: vec![],
                },
            ) == Some(true);
            vec![node.k, if kept { node.k } else { i64::MIN }]
        })
        .collect();
    expect_bag(
        runner,
        &format!("MATCH (n) OPTIONAL MATCH (m) WHERE m = n AND {with_m} RETURN n.k, m.k"),
        optional,
    )?;
    let counted = runner.run(&format!(
        "MATCH (n) RETURN count(CASE WHEN {text} THEN 1 END) AS c, count(*) AS all"
    ))?;
    let want = vec![vec![
        Value::Integer(yes.len() as i64),
        Value::Integer(graph.nodes.len() as i64),
    ]];
    if counted != want {
        return Err(format!("count of {text}: got {counted:?}, want {want:?}"));
    }
    Ok(())
}

fn check_projection(
    runner: &mut dyn Runner,
    graph: &Graph,
    choices: &mut Rng,
) -> Result<(), String> {
    let scope = node_scope();
    let ty = *choices.pick(&[Ty::Int, Ty::Str, Ty::Bool, Ty::Any]);
    let e = expr(ty, 3, &scope, choices);
    let text = render(&e, &scope);
    let query = format!("MATCH (n) RETURN n.k, {text} ORDER BY n.k");
    let rows = runner.run(&query)?;
    let want: Vec<Vec<Value>> = graph
        .nodes
        .values()
        .map(|node| {
            vec![
                Value::Integer(node.k),
                eval(
                    &e,
                    &Row {
                        nodes: vec![node],
                        rels: vec![],
                    },
                )
                .value(),
            ]
        })
        .collect();
    let matches = rows.len() == want.len()
        && rows.iter().zip(&want).all(|(got, want)| {
            got.len() == 2 && same(&got[0], &want[0]) && same(&got[1], &want[1])
        });
    if !matches {
        return Err(format!("{query}\n  got  {rows:?}\n  want {want:?}"));
    }
    Ok(())
}

fn check_order(runner: &mut dyn Runner, graph: &Graph, choices: &mut Rng) -> Result<(), String> {
    let scope = node_scope();
    let p = expr(Ty::Bool, 2, &scope, choices);
    let key = expr(Ty::Any, 2, &scope, choices);
    let descending = choices.chance(1, 2);
    let nulls_first = match choices.below(3) {
        0 => None,
        1 => Some(true),
        _ => Some(false),
    };
    let skip = choices.below(4) as usize;
    let limit = choices.below(6) as usize;
    let mut rows: Vec<(V, i64)> = graph
        .nodes
        .values()
        .filter(|node| {
            holds(
                &p,
                &Row {
                    nodes: vec![node],
                    rels: vec![],
                },
            ) == Some(true)
        })
        .map(|node| {
            (
                eval(
                    &key,
                    &Row {
                        nodes: vec![node],
                        rels: vec![],
                    },
                ),
                node.k,
            )
        })
        .collect();
    rows.sort_by(|a, b| {
        let null = |value: &V| matches!(value, V::Null);
        let by_key = match (nulls_first, null(&a.0), null(&b.0)) {
            (Some(first), true, false) => {
                if first {
                    Ordering::Less
                } else {
                    Ordering::Greater
                }
            }
            (Some(first), false, true) => {
                if first {
                    Ordering::Greater
                } else {
                    Ordering::Less
                }
            }
            _ if descending => order(&a.0, &b.0).reverse(),
            _ => order(&a.0, &b.0),
        };
        by_key.then(a.1.cmp(&b.1))
    });
    let want: Vec<i64> = rows
        .iter()
        .skip(skip)
        .take(limit)
        .map(|row| row.1)
        .collect();
    let (p, key) = (render(&p, &scope), render(&key, &scope));
    let direction = match (descending, nulls_first) {
        (false, None) => "ASC",
        (true, None) => "DESC",
        (false, Some(true)) => "NULLS FIRST",
        (true, Some(true)) => "DESC NULLS FIRST",
        (false, Some(false)) => "ASC NULLS LAST",
        (true, Some(false)) => "DESCENDING NULLS LAST",
    };
    let queries = [
        format!(
            "MATCH (n) WHERE {p} RETURN n.k ORDER BY {key} {direction}, n.k SKIP {skip} LIMIT {limit}"
        ),
        format!(
            "MATCH (n) WHERE {p} WITH n ORDER BY {key} {direction}, n.k SKIP {skip} LIMIT {limit} RETURN n.k"
        ),
        format!(
            "MATCH (n) WHERE {p} RETURN n.k AS k, {key} AS v ORDER BY v {direction}, k OFFSET {skip} LIMIT {limit}"
        ),
    ];
    let query = choices.pick(&queries);
    let got: Vec<i64> = runner
        .run(query)?
        .iter()
        .map(|row| match row.first() {
            Some(Value::Integer(k)) => Ok(*k),
            other => Err(format!("{query}: unexpected {other:?}")),
        })
        .collect::<Result<_, _>>()?;
    if got != want {
        return Err(format!("{query}\n  got  {got:?}\n  want {want:?}"));
    }
    Ok(())
}

struct Group {
    key: V,
    keys: Vec<i64>,
    xs: Vec<i64>,
    ys: Vec<String>,
}

fn check_grouping(runner: &mut dyn Runner, graph: &Graph, choices: &mut Rng) -> Result<(), String> {
    let scope = node_scope();
    let p = expr(Ty::Bool, 2, &scope, choices);
    let g = expr(Ty::Any, 2, &scope, choices);
    let mut groups: Vec<Group> = Vec::new();
    for node in graph.nodes.values() {
        let row = Row {
            nodes: vec![node],
            rels: vec![],
        };
        if holds(&p, &row) != Some(true) {
            continue;
        }
        let key = eval(&g, &row);
        let index = match groups
            .iter()
            .position(|group| order(&group.key, &key) == Ordering::Equal)
        {
            Some(index) => index,
            None => {
                groups.push(Group {
                    key,
                    keys: vec![],
                    xs: vec![],
                    ys: vec![],
                });
                groups.len() - 1
            }
        };
        let group = &mut groups[index];
        group.keys.push(node.k);
        if let Some(V::Int(x)) = node.props.get("x") {
            group.xs.push(*x);
        }
        if let Some(V::Str(y)) = node.props.get("y") {
            group.ys.push(y.clone());
        }
    }
    let (p_text, g_text) = (render(&p, &scope), render(&g, &scope));
    let query = format!(
        "MATCH (n) WHERE {p_text} RETURN {g_text} AS g, count(*) AS c, count(n.x) AS cx, sum(n.x) AS s, \
         min(n.y) AS mn, max(n.y) AS mx, \
         reduce(acc = [], k IN collect(n.k) | acc + [k]) AS ks"
    );
    let rows = runner.run(&query)?;
    let fail = |why: &str| Err(format!("{query}: {why}\n  got {rows:?}"));
    if rows.len() != groups.len() {
        return fail(&format!("{} groups, want {}", rows.len(), groups.len()));
    }
    for row in &rows {
        let [g, c, cx, s, mn, mx, ks] = row.as_slice() else {
            return fail("row width");
        };
        let key = to_v(g)?;
        let Some(group) = groups
            .iter()
            .find(|group| order(&group.key, &key) == Ordering::Equal)
        else {
            return fail(&format!("unexpected group {g:?}"));
        };
        let mut got_keys = match to_v(ks)? {
            V::List(items) => items
                .iter()
                .map(|item| match item {
                    V::Int(k) => Ok(*k),
                    _ => Err("collect".to_owned()),
                })
                .collect::<Result<Vec<_>, _>>()?,
            _ => return fail("collect is not a list"),
        };
        got_keys.sort();
        let want_min = group
            .ys
            .iter()
            .min()
            .map_or(Value::Null, |y| Value::String(y.clone()));
        let want_max = group
            .ys
            .iter()
            .max()
            .map_or(Value::Null, |y| Value::String(y.clone()));
        let ok = got_keys == group.keys
            && *c == Value::Integer(group.keys.len() as i64)
            && *cx == Value::Integer(group.xs.len() as i64)
            && *s == Value::Integer(group.xs.iter().sum())
            && *mn == want_min
            && *mx == want_max;
        if !ok {
            return fail(&format!(
                "group {g:?}: want keys {:?} xs {:?} ys {:?}",
                group.keys, group.xs, group.ys
            ));
        }
    }
    let distinct = runner.run(&format!(
        "MATCH (n) WHERE {p_text} RETURN DISTINCT {g_text} AS g"
    ))?;
    if distinct.len() != groups.len() {
        return Err(format!(
            "DISTINCT {g_text} where {p_text}: {} rows, want {}",
            distinct.len(),
            groups.len()
        ));
    }
    let non_null = groups
        .iter()
        .filter(|group| !matches!(group.key, V::Null))
        .count() as i64;
    let counted = runner.run(&format!(
        "MATCH (n) WHERE {p_text} RETURN count(DISTINCT {g_text})"
    ))?;
    if counted != vec![vec![Value::Integer(non_null)]] {
        return Err(format!(
            "count(DISTINCT {g_text}) where {p_text}: got {counted:?}, want {non_null}"
        ));
    }
    Ok(())
}

fn check_optional(runner: &mut dyn Runner, graph: &Graph, choices: &mut Rng) -> Result<(), String> {
    let scope = pattern_scope();
    let q = expr(Ty::Bool, 2, &scope, choices);
    let text = render(&q, &scope);
    let mut want = Vec::new();
    let mut degrees = Vec::new();
    for a in graph.nodes.values() {
        let matched: Vec<&RelM> = graph
            .rels
            .values()
            .filter(|rel| rel.src == a.k)
            .filter(|rel| {
                let row = Row {
                    nodes: vec![a, &graph.nodes[&rel.dst]],
                    rels: vec![rel],
                };
                holds(&q, &row) == Some(true)
            })
            .collect();
        if matched.is_empty() {
            want.push(vec![a.k, i64::MIN, i64::MIN]);
        }
        for rel in &matched {
            want.push(vec![a.k, rel.k, rel.dst]);
        }
        degrees.push(vec![a.k, matched.len() as i64]);
    }
    expect_bag(
        runner,
        &format!("MATCH (a) OPTIONAL MATCH (a)-[r]->(b) WHERE {text} RETURN a.k, r.k, b.k"),
        want,
    )?;
    expect_bag(
        runner,
        &format!("MATCH (a) OPTIONAL MATCH (a)-[r]->(b) WHERE {text} RETURN a.k, count(r)"),
        degrees.clone(),
    )?;
    expect_bag(
        runner,
        &format!("MATCH (a) RETURN a.k, size([(a)-[r]->(b) WHERE {text} | r.k])"),
        degrees,
    )
}

fn check_pattern(runner: &mut dyn Runner, graph: &Graph, choices: &mut Rng) -> Result<(), String> {
    let scope = pattern_scope();
    let q = expr(Ty::Bool, 2, &scope, choices);
    let text = render(&q, &scope);
    let directed = |ty: Option<&str>| -> Vec<Vec<i64>> {
        graph
            .rels
            .values()
            .filter(|rel| ty.is_none_or(|ty| rel.ty == ty))
            .filter(|rel| {
                let row = Row {
                    nodes: vec![&graph.nodes[&rel.src], &graph.nodes[&rel.dst]],
                    rels: vec![rel],
                };
                holds(&q, &row) == Some(true)
            })
            .map(|rel| vec![rel.src, rel.k, rel.dst])
            .collect()
    };
    let all = directed(None);
    let variants = [
        format!("MATCH (a)-[r]->(b) WHERE {text} RETURN a.k, r.k, b.k"),
        format!("MATCH (b)<-[r]-(a) WHERE {text} RETURN a.k, r.k, b.k"),
        format!("MATCH (a) MATCH (a)-[r]->(b) WHERE {text} RETURN a.k, r.k, b.k"),
        format!("MATCH (b) MATCH (a)-[r]->(b) WHERE {text} RETURN a.k, r.k, b.k"),
        format!("MATCH (a)-[r:R|S]->(b) WHERE {text} RETURN a.k, r.k, b.k"),
        format!("MATCH (a)-[r]->(b) WITH a, r, b WHERE {text} RETURN a.k, r.k, b.k"),
        format!("MATCH (a), (b) MATCH (a)-[r]->(b) WHERE {text} RETURN a.k, r.k, b.k"),
        format!("MATCH ()-[r]->() MATCH (a)-[r]->(b) WHERE {text} RETURN a.k, r.k, b.k"),
    ];
    expect_bag(runner, choices.pick(&variants), all.clone())?;
    let ty = *choices.pick(&TYPES);
    expect_bag(
        runner,
        &format!("MATCH (a)-[r:{ty}]->(b) WHERE {text} RETURN a.k, r.k, b.k"),
        directed(Some(ty)),
    )?;
    let undirected: Vec<Vec<i64>> = graph
        .rels
        .values()
        .filter(|rel| rel.src != rel.dst)
        .flat_map(|rel| [(rel.src, rel.dst), (rel.dst, rel.src)].map(|ends| (rel, ends)))
        .filter(|(rel, (a, b))| {
            let row = Row {
                nodes: vec![&graph.nodes[a], &graph.nodes[b]],
                rels: vec![rel],
            };
            holds(&q, &row) == Some(true)
        })
        .map(|(rel, (a, b))| vec![a, rel.k, b])
        .collect();
    expect_bag(
        runner,
        &format!("MATCH (a)-[r]-(b) WHERE a <> b AND {text} RETURN a.k, r.k, b.k"),
        undirected,
    )?;
    let mut pairs: Vec<Vec<i64>> = all.iter().map(|row| vec![row[0], row[2]]).collect();
    pairs.sort();
    pairs.dedup();
    expect_bag(
        runner,
        &format!(
            "MATCH (a), (b) WHERE EXISTS {{ MATCH (a)-[r]->(b) WHERE {text} }} RETURN a.k, b.k"
        ),
        pairs,
    )?;
    let mut counts: Vec<Vec<i64>> = Vec::new();
    let mut sums: Vec<Vec<i64>> = Vec::new();
    for a in graph.nodes.keys() {
        for b in graph.nodes.keys() {
            let between = all.iter().filter(|row| row[0] == *a && row[2] == *b);
            counts.push(vec![*a, *b, between.clone().count() as i64]);
            sums.push(vec![*a, *b, between.map(|row| row[1]).sum()]);
        }
    }
    let counted = [
        format!("MATCH (a), (b) RETURN a.k, b.k, COUNT {{ (a)-[r]->(b) WHERE {text} }}"),
        format!("MATCH (a), (b) RETURN a.k, b.k, COUNT {{ MATCH (a)-[r]->(b) WHERE {text} }}"),
        format!(
            "MATCH (a), (b) RETURN a.k, b.k, COUNT {{ MATCH (a)-[r]->(b) WHERE {text} RETURN r }}"
        ),
        format!(
            "MATCH (a), (b) CALL (a, b) {{ MATCH (a)-[r]->(b) WHERE {text} RETURN count(r) AS c }} \
             RETURN a.k, b.k, c"
        ),
    ];
    expect_bag(runner, choices.pick(&counted), counts)?;
    let summed = [
        format!(
            "MATCH (a), (b) RETURN a.k, b.k, \
             reduce(s = 0, k IN COLLECT {{ MATCH (a)-[r]->(b) WHERE {text} RETURN r.k }} | s + k)"
        ),
        format!(
            "MATCH (a), (b) OPTIONAL CALL {{ WITH a, b MATCH (a)-[r]->(b) WHERE {text} \
             RETURN r.k AS rk UNION ALL RETURN 0 AS rk }} RETURN a.k, b.k, sum(rk)"
        ),
    ];
    expect_bag(runner, choices.pick(&summed), sums)?;
    Ok(())
}

fn trails(
    graph: &Graph,
    ty: Option<&str>,
    w: Option<i64>,
    undirected: bool,
    lo: usize,
    hi: usize,
) -> Vec<Vec<i64>> {
    let mut out = Vec::new();
    let mut stack: Vec<(i64, i64, Vec<i64>)> =
        graph.nodes.keys().map(|k| (*k, *k, vec![])).collect();
    while let Some((start, at, path)) = stack.pop() {
        if path.len() >= lo {
            let mut packed = 0i64;
            for k in &path {
                packed = packed * 1000 + k + 1;
            }
            out.push(vec![start, packed, at]);
        }
        if path.len() == hi {
            continue;
        }
        for rel in graph.rels.values() {
            if path.contains(&rel.k) || ty.is_some_and(|ty| rel.ty != ty) {
                continue;
            }
            if let Some(w) = w {
                let equal = matches!(rel.props.get("w"), Some(V::Int(value)) if *value == w);
                if !equal {
                    continue;
                }
            }
            let mut next = |to: i64| {
                let mut extended = path.clone();
                extended.push(rel.k);
                stack.push((start, to, extended));
            };
            if rel.src == at {
                next(rel.dst);
            }
            if undirected && rel.dst == at && rel.src != rel.dst {
                next(rel.src);
            }
        }
    }
    out
}

fn check_paths(runner: &mut dyn Runner, graph: &Graph, choices: &mut Rng) -> Result<(), String> {
    if graph.rels.len() > 40 {
        return Ok(());
    }
    let lo = choices.below(3) as usize;
    let hi = lo + choices.below(3) as usize;
    let ty = if choices.chance(1, 2) {
        None
    } else {
        Some(*choices.pick(&TYPES))
    };
    let w = if choices.chance(1, 3) {
        Some(choices.below(3) as i64 - 1)
    } else {
        None
    };
    let undirected = choices.chance(1, 3) && !graph.has_loop();
    let ty_text = ty.map_or(String::new(), |ty| format!(":{ty}"));
    let w_text = w.map_or(String::new(), |w| format!(" {{w: {w}}}"));
    let (left, right) = if undirected { ("-", "-") } else { ("-", "->") };
    let want = trails(graph, ty, w, undirected, lo, hi);
    let query = format!(
        "MATCH (a){left}[rs{ty_text}*{lo}..{hi}{w_text}]{right}(b) RETURN a.k, [x IN rs | x.k], b.k"
    );
    expect_bag(runner, &query, want.clone())?;
    let path_query = format!(
        "MATCH p = (a){left}[{ty_text}*{lo}..{hi}{w_text}]{right}(b) \
         RETURN a.k, [x IN relationships(p) | x.k], b.k, length(p), size(nodes(p))"
    );
    let rows = runner.run(&path_query)?;
    for row in &rows {
        let ok = match (&row[1], &row[3], &row[4]) {
            (Value::List(rels), Value::Integer(length), Value::Integer(nodes)) => {
                rels.len() as i64 == *length && *nodes == length + 1
            }
            _ => false,
        };
        if !ok {
            return Err(format!("{path_query}: inconsistent row {row:?}"));
        }
    }
    let shortened: Vec<Vec<Value>> = rows.iter().map(|row| row[..3].to_vec()).collect();
    let mut got = ints_sorted(&shortened)?;
    let mut want = want;
    got.sort();
    want.sort();
    if got != want {
        return Err(format!("{path_query}\n  got  {got:?}\n  want {want:?}"));
    }
    if !undirected && ty.is_none() && w.is_none() {
        let two: Vec<Vec<i64>> = trails(graph, None, None, false, 2, 2)
            .into_iter()
            .map(|row| vec![row[0], row[2]])
            .collect();
        expect_bag(runner, "MATCH (a)-[r1]->(m)-[r2]->(b) RETURN a.k, b.k", two)?;
    }
    Ok(())
}

fn check_update(tx: &mut dyn Runner, graph: &Graph, choices: &mut Rng) -> Result<(), String> {
    let scope = node_scope();
    let p = expr(Ty::Bool, 2, &scope, choices);
    let text = render(&p, &scope);
    let yes = keys_where(graph, &p, Some(true), None);
    let set_forms = [
        format!("MATCH (n) WHERE {text} SET n.flag = true RETURN count(*)"),
        format!("MATCH (n) WHERE {text} CALL (n) {{ SET n.flag = true }} RETURN count(*)"),
        format!("MATCH (n) WHERE {text} SET n.flag = true RETURN n NEXT RETURN count(*)"),
    ];
    let set = tx.run(choices.pick(&set_forms))?;
    if set != vec![vec![Value::Integer(yes.len() as i64)]] {
        return Err(format!("SET where {text}: got {set:?}, want {}", yes.len()));
    }
    expect_bag(tx, "MATCH (n) WHERE n.flag RETURN n.k", yes.clone())?;
    tx.run("MATCH (n) REMOVE n.flag")?;
    let delete_forms = [
        format!("MATCH (n) WHERE {text} DETACH DELETE n"),
        format!("MATCH (n) WHERE {text} CALL (n) {{ DETACH DELETE n }}"),
    ];
    tx.run(choices.pick(&delete_forms))?;
    let left = keys_where(graph, &p, Some(false), None)
        .into_iter()
        .chain(keys_where(graph, &p, None, None))
        .collect::<Vec<_>>();
    expect_bag(tx, "MATCH (n) RETURN n.k", left.clone())?;
    let kept: Vec<Vec<i64>> = graph
        .rels
        .values()
        .filter(|rel| left.contains(&vec![rel.src]) && left.contains(&vec![rel.dst]))
        .map(|rel| vec![rel.k])
        .collect();
    expect_bag(tx, "MATCH ()-[r]->() RETURN r.k", kept)?;
    Ok(())
}

fn check_state(runner: &mut dyn Runner, graph: &Graph) -> Result<(), String> {
    let query = "MATCH (n) RETURN n.k, labels(n), n.x, n.y, n.z, n.t ORDER BY n.k";
    let rows = runner.run(query)?;
    let want: Rows = graph
        .nodes
        .values()
        .map(|node| {
            let labels = node
                .labels
                .iter()
                .map(|label| Value::String((*label).to_owned()))
                .collect();
            let mut row = vec![Value::Integer(node.k), Value::List(labels)];
            for key in ["x", "y", "z", "t"] {
                row.push(node.props.get(key).map_or(Value::Null, V::value));
            }
            row
        })
        .collect();
    let mut sorted_rows = rows.clone();
    for row in &mut sorted_rows {
        if let Value::List(labels) = &mut row[1] {
            labels.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
        }
    }
    let matches = sorted_rows.len() == want.len()
        && sorted_rows
            .iter()
            .zip(&want)
            .all(|(got, want)| got.iter().zip(want).all(|(a, b)| same(a, b)));
    if !matches {
        return Err(format!("{query}\n  got  {rows:?}\n  want {want:?}"));
    }
    let rels: Vec<Vec<i64>> = graph
        .rels
        .values()
        .map(|rel| vec![rel.src, rel.k, rel.dst])
        .collect();
    expect_bag(runner, "MATCH (a)-[r]->(b) RETURN a.k, r.k, b.k", rels)?;
    let weights: Vec<Vec<i64>> = graph
        .rels
        .values()
        .map(|rel| {
            let w = match rel.props.get("w") {
                Some(V::Int(w)) => *w,
                _ => i64::MIN,
            };
            vec![rel.k, w]
        })
        .collect();
    expect_bag(runner, "MATCH ()-[r]->() RETURN r.k, r.w", weights)
}

fn check_round(runner: &mut dyn Runner, graph: &Graph, choices: &mut Rng) -> Result<(), String> {
    check_filter(runner, graph, choices)?;
    check_projection(runner, graph, choices)?;
    check_order(runner, graph, choices)?;
    check_grouping(runner, graph, choices)?;
    check_pattern(runner, graph, choices)?;
    check_optional(runner, graph, choices)?;
    check_paths(runner, graph, choices)
}

const INDEXES: [(&str, IndexTarget<'static>, &str); 6] = [
    ("ax", IndexTarget::Label("A"), "x"),
    ("ay", IndexTarget::Label("A"), "y"),
    ("az", IndexTarget::Label("A"), "z"),
    ("bt", IndexTarget::Label("B"), "t"),
    ("bz", IndexTarget::Label("B"), "z"),
    ("rw", IndexTarget::RelationshipType("R"), "w"),
];

struct Session {
    db: Database,
    graph: Graph,
    next_key: i64,
    log: String,
}

impl Session {
    fn edit_committed(&mut self, rng: &mut Rng) -> Result<(), String> {
        let mut tx = self.db.begin_write().map_err(|error| error.to_string())?;
        let mut graph = self.graph.clone();
        let mut next_key = self.next_key;
        for _ in 0..1 + rng.below(6) {
            let query = edit(&mut graph, &mut next_key, rng);
            self.log.push_str(&query);
            self.log.push('\n');
            tx.run(&query)?;
        }
        if rng.chance(1, 6) {
            self.log.push_str("-- rollback\n");
            tx.rollback().map_err(|error| error.to_string())?;
        } else {
            self.log.push_str("-- commit\n");
            tx.commit().map_err(|error| error.to_string())?;
            self.graph = graph;
            self.next_key = next_key;
        }
        Ok(())
    }

    fn round(&mut self, rng: &mut Rng) -> Result<(), String> {
        for _ in 0..1 + rng.below(4) {
            self.edit_committed(rng)?;
        }
        if rng.chance(1, 4) {
            let (name, target, property) = rng.pick(&INDEXES);
            self.log.push_str(&format!("-- create index {name}\n"));
            self.db
                .create_property_index(name, *target, property)
                .map_err(|error| error.to_string())?;
        }
        if rng.chance(1, 8) {
            let (name, _, _) = rng.pick(&INDEXES);
            self.log.push_str(&format!("-- drop index {name}\n"));
            self.db
                .drop_index(name)
                .map_err(|error| error.to_string())?;
        }
        let mut db = &self.db;
        check_state(&mut db, &self.graph)?;
        check_round(&mut db, &self.graph, rng)?;
        let mut tx = self.db.begin_write().map_err(|error| error.to_string())?;
        let mut graph = self.graph.clone();
        let mut next_key = self.next_key;
        for _ in 0..rng.below(5) {
            let query = edit(&mut graph, &mut next_key, rng);
            self.log.push_str(&format!("-- open: {query}\n"));
            tx.run(&query)?;
        }
        check_state(&mut tx, &graph)?;
        check_round(&mut tx, &graph, rng)?;
        check_update(&mut tx, &graph, rng)?;
        tx.rollback().map_err(|error| error.to_string())?;
        let mut db = &self.db;
        check_state(&mut db, &self.graph)
    }
}

fn gap(db: &Database, rng: &mut Rng) -> Result<(), String> {
    let mut runner = db;
    if rng.chance(1, 3) {
        runner.run("UNWIND range(1, 4100) AS i INSERT (:Gap)")?;
        runner.run("MATCH (n:Gap) DELETE n")?;
    }
    Ok(())
}

fn fail(seed: u64, log: &str, reason: &str) -> ! {
    panic!("seed {seed} (replay with RHIZODB_TEST_SEED={seed}): {reason}\nstatements:\n{log}")
}

fn session(db: Database, rng: &mut Rng) -> Result<Session, String> {
    gap(&db, rng)?;
    Ok(Session {
        db,
        graph: Graph::default(),
        next_key: 1,
        log: String::new(),
    })
}

fn in_memory_sessions(salt: u64, open: impl Fn() -> Database) {
    let base = seed();
    for offset in 0..cases(FAST_GRAPHS) {
        let seed = base.wrapping_add(offset).wrapping_add(salt);
        let mut rng = Rng::new(seed);
        let mut session = match session(open(), &mut rng) {
            Ok(session) => session,
            Err(reason) => fail(seed, "", &reason),
        };
        for _ in 0..ROUNDS_PER_GRAPH {
            if let Err(reason) = session.round(&mut rng) {
                fail(seed, &session.log, &reason);
            }
        }
    }
}

#[test]
fn random_queries_in_memory_match_the_reference() {
    in_memory_sessions(0, db);
}

#[test]
fn random_queries_split_across_threads_match_the_reference() {
    in_memory_sessions(2 << 40, || parallel_options().in_memory().unwrap());
}

#[test]
fn random_queries_on_a_reopened_file_match_the_reference() {
    let base = seed();
    let directory = tempfile::tempdir().unwrap();
    for offset in 0..cases(FAST_GRAPHS / 4) {
        let seed = base.wrapping_add(offset).wrapping_add(1 << 40);
        let mut rng = Rng::new(seed);
        let path = directory.path().join(format!("oracle-{offset}.rhizodb"));
        let mut session = match session(options().open(&path).unwrap(), &mut rng) {
            Ok(session) => session,
            Err(reason) => fail(seed, "", &reason),
        };
        for _ in 0..ROUNDS_PER_GRAPH {
            if rng.chance(1, 3) {
                session.log.push_str("-- reopen\n");
                drop(std::mem::replace(&mut session.db, db()));
                session.db = options().open(&path).unwrap();
            }
            if let Err(reason) = session.round(&mut rng) {
                fail(seed, &session.log, &reason);
            }
        }
    }
}
