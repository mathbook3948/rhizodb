mod support;

use std::collections::BTreeMap;

use proptest::prelude::*;
use rhizodb::Database;
use rhizodb::Parameters;
use rhizodb::Value;
use support::*;

fn graph(nodes: usize, edges: &[(usize, usize)]) -> Database {
    let db = db();
    for id in 0..nodes {
        run(&db, &format!("INSERT (:N {{id: {id}}})"));
    }
    for (at, (from, to)) in edges.iter().enumerate() {
        run(
            &db,
            &format!(
                "MATCH (a:N {{id: {from}}}), (b:N {{id: {to}}}) INSERT (a)-[:R {{e: {at}}}]->(b)"
            ),
        );
    }
    db
}

/// The moves from each node: the far node and the edge, out of each node only, or both ways
/// with a self loop once.
fn moves(nodes: usize, edges: &[(usize, usize)], both: bool) -> Vec<Vec<(usize, usize)>> {
    let mut moves = vec![Vec::new(); nodes];
    for (at, (from, to)) in edges.iter().enumerate() {
        moves[*from].push((*to, at));
        if both && from != to {
            moves[*to].push((*from, at));
        }
    }
    moves
}

/// The length and number of the shortest walks of one hop or more from `from` to each node.
fn shortest_walks(moves: &[Vec<(usize, usize)>], from: usize) -> BTreeMap<usize, (usize, u64)> {
    let mut found = BTreeMap::new();
    let mut counts = vec![0u64; moves.len()];
    counts[from] = 1;
    for length in 1..=moves.len() {
        let mut next = vec![0u64; moves.len()];
        for (node, count) in counts.iter().enumerate() {
            for (other, _) in &moves[node] {
                next[*other] += count;
            }
        }
        for (node, count) in next.iter().enumerate() {
            if *count > 0 {
                found.entry(node).or_insert((length, *count));
            }
        }
        counts = next;
    }
    found
}

/// The length and number of the shortest trails of one hop or more from `from` to each node.
fn shortest_trails(moves: &[Vec<(usize, usize)>], from: usize) -> BTreeMap<usize, (usize, u64)> {
    let mut found: BTreeMap<usize, (usize, u64)> = BTreeMap::new();
    let mut stack = vec![(from, Vec::<usize>::new())];
    while let Some((node, used)) = stack.pop() {
        for (other, edge) in &moves[node] {
            if used.contains(edge) {
                continue;
            }
            let mut walked = used.clone();
            walked.push(*edge);
            let entry = found.entry(*other).or_insert((walked.len(), 0));
            if walked.len() < entry.0 {
                *entry = (walked.len(), 1);
            } else if walked.len() == entry.0 {
                entry.1 += 1;
            }
            stack.push((*other, walked));
        }
    }
    found
}

/// The number of walks of each length from `from` to each node, up to four times as many
/// hops as nodes, which holds the third shortest walk of a small graph.
fn walk_lengths(moves: &[Vec<(usize, usize)>], from: usize) -> BTreeMap<usize, Vec<(usize, u64)>> {
    let mut found: BTreeMap<usize, Vec<(usize, u64)>> = BTreeMap::new();
    let mut counts = vec![0u64; moves.len()];
    counts[from] = 1;
    for length in 1..=4 * moves.len() {
        let mut next = vec![0u64; moves.len()];
        for (node, count) in counts.iter().enumerate() {
            for (other, _) in &moves[node] {
                next[*other] = next[*other].saturating_add(*count);
            }
        }
        for (node, count) in next.iter().enumerate() {
            if *count > 0 {
                found.entry(node).or_default().push((length, *count));
            }
        }
        counts = next;
    }
    found
}

fn trail_lengths(moves: &[Vec<(usize, usize)>], from: usize) -> BTreeMap<usize, Vec<(usize, u64)>> {
    let mut lengths: BTreeMap<usize, BTreeMap<usize, u64>> = BTreeMap::new();
    let mut stack = vec![(from, Vec::<usize>::new())];
    while let Some((node, used)) = stack.pop() {
        for (other, edge) in &moves[node] {
            if used.contains(edge) {
                continue;
            }
            let mut walked = used.clone();
            walked.push(*edge);
            *lengths
                .entry(*other)
                .or_default()
                .entry(walked.len())
                .or_default() += 1;
            stack.push((*other, walked));
        }
    }
    lengths
        .into_iter()
        .map(|(node, counts)| (node, counts.into_iter().collect()))
        .collect()
}

/// The number of paths of each length from `from` to each node that repeat no node, or with
/// `closed` no node but the start at the end.
fn node_path_lengths(moves: &[Vec<(usize, usize)>], from: usize, closed: bool) -> Lengths {
    let mut lengths: BTreeMap<usize, BTreeMap<usize, u64>> = BTreeMap::new();
    let mut stack = vec![(from, vec![from], 0usize)];
    while let Some((node, visited, length)) = stack.pop() {
        for (other, _) in &moves[node] {
            let back = closed && *other == from;
            if visited.contains(other) && !back {
                continue;
            }
            *lengths
                .entry(*other)
                .or_default()
                .entry(length + 1)
                .or_default() += 1;
            if !back {
                let mut visited = visited.clone();
                visited.push(*other);
                stack.push((*other, visited, length + 1));
            }
        }
    }
    lengths
        .into_iter()
        .map(|(node, counts)| (node, counts.into_iter().collect()))
        .collect()
}

/// The lengths `SHORTEST k` keeps of the counts by length, shortest first.
fn shortest(counts: &[(usize, u64)], paths: u64) -> Vec<usize> {
    let mut kept = Vec::new();
    for (length, count) in counts {
        for _ in 0..*count {
            if kept.len() as u64 == paths {
                return kept;
            }
            kept.push(*length);
        }
    }
    kept
}

/// The lengths `SHORTEST k GROUPS` keeps.
fn groups(counts: &[(usize, u64)], groups: usize) -> Vec<usize> {
    counts
        .iter()
        .take(groups)
        .flat_map(|(length, count)| std::iter::repeat_n(*length, *count as usize))
        .collect()
}

fn lengths(db: &Database, pattern: &str, from: usize) -> BTreeMap<usize, Vec<usize>> {
    let query = format!("MATCH (a:N {{id: $from}}) MATCH p = {pattern} RETURN b.id, length(p)");
    let parameters = Parameters::from([("from", from as i64)]);
    let mut found: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for row in db.execute(&query, &parameters).unwrap().rows() {
        match row.as_slice() {
            [Value::Integer(end), Value::Integer(length)] => {
                found
                    .entry(*end as usize)
                    .or_default()
                    .push(*length as usize);
            }
            other => panic!("{query}: {other:?}"),
        }
    }
    for lengths in found.values_mut() {
        lengths.sort_unstable();
    }
    found
}

type Lengths = BTreeMap<usize, Vec<(usize, u64)>>;

type Keep = fn(&[(usize, u64)]) -> Vec<usize>;

fn found(db: &Database, query: &str, from: usize) -> BTreeMap<usize, (usize, u64)> {
    let parameters = Parameters::from([("from", from as i64)]);
    let rows = db.execute(query, &parameters).unwrap();
    rows.rows()
        .iter()
        .map(|row| match row.as_slice() {
            [
                Value::Integer(end),
                Value::Integer(length),
                Value::Integer(count),
            ] => (*end as usize, (*length as usize, *count as u64)),
            other => panic!("{query}: {other:?}"),
        })
        .collect()
}

fn all_shortest(db: &Database, pattern: &str, from: usize) -> BTreeMap<usize, (usize, u64)> {
    let query = format!(
        "MATCH (a:N {{id: $from}}) MATCH p = {pattern} RETURN b.id, min(length(p)), count(*)"
    );
    found(db, &query, from)
}

fn between(db: &Database, pattern: &str, from: usize, to: usize) -> Option<(usize, u64)> {
    let query = format!(
        "MATCH (a:N {{id: $from}}), (b:N {{id: {to}}}) MATCH p = {pattern} RETURN b.id, min(length(p)), count(*)"
    );
    found(db, &query, from).remove(&to)
}

fn edges() -> impl Strategy<Value = (usize, Vec<(usize, usize)>)> {
    (2usize..=6).prop_flat_map(|nodes| {
        (
            Just(nodes),
            prop::collection::vec((0..nodes, 0..nodes), 0..=8),
        )
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn every_search_finds_the_shortest_paths_a_reference_counts((nodes, edges) in edges()) {
        let db = graph(nodes, &edges);
        for (both, arrow) in [(false, "-[:R]->"), (true, "-[:R]-")] {
            let moves = moves(nodes, &edges, both);
            for from in 0..nodes {
                let walks = shortest_walks(&moves, from);
                let trails = shortest_trails(&moves, from);
                let single = format!("ALL SHORTEST (a){arrow}+(b)");
                let split = format!("ALL SHORTEST (a){arrow}(m){arrow}*(b)");
                let trail_split = format!("ALL SHORTEST TRAIL (a){arrow}(m){arrow}*(b)");
                let trail = format!("ALL SHORTEST TRAIL (a){arrow}+(b)");
                prop_assert_eq!(&all_shortest(&db, &single, from), &walks, "{} from {}", single, from);
                prop_assert_eq!(&all_shortest(&db, &split, from), &walks, "{} from {}", split, from);
                prop_assert_eq!(&all_shortest(&db, &trail, from), &trails, "{} from {}", trail, from);
                prop_assert_eq!(&all_shortest(&db, &trail_split, from), &trails, "{} from {}", trail_split, from);
                for to in 0..nodes {
                    let expected = walks.get(&to).copied();
                    prop_assert_eq!(between(&db, &single, from, to), expected, "{} {}->{}", single, from, to);
                    let any = between(&db, &format!("ANY SHORTEST (a){arrow}+(b)"), from, to);
                    prop_assert_eq!(any, expected.map(|(length, _)| (length, 1)));
                    prop_assert_eq!(between(&db, &trail, from, to), trails.get(&to).copied(), "{} {}->{}", trail, from, to);
                }
            }
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn counted_selectors_keep_the_shortest_paths_a_reference_counts((nodes, edges) in edges()) {
        let db = graph(nodes, &edges);
        for (both, arrow) in [(false, "-[:R]->"), (true, "-[:R]-")] {
            let moves = moves(nodes, &edges, both);
            for from in 0..nodes {
                let walks = walk_lengths(&moves, from);
                let trails = trail_lengths(&moves, from);
                let acyclic = node_path_lengths(&moves, from, false);
                let simple = node_path_lengths(&moves, from, true);
                let cases: [(String, &Lengths, Keep); 10] = [
                    (format!("ALL ACYCLIC (a){arrow}+(b)"), &acyclic, |counts| groups(counts, usize::MAX)),
                    (format!("SHORTEST 2 ACYCLIC (a){arrow}+(b)"), &acyclic, |counts| shortest(counts, 2)),
                    (format!("ALL SIMPLE (a){arrow}+(b)"), &simple, |counts| groups(counts, usize::MAX)),
                    (format!("ANY SHORTEST SIMPLE (a){arrow}(m){arrow}*(b)"), &simple, |counts| shortest(counts, 1)),
                    (format!("SHORTEST 3 (a){arrow}+(b)"), &walks, |counts| shortest(counts, 3)),
                    (format!("SHORTEST 3 (a){arrow}(m){arrow}*(b)"), &walks, |counts| shortest(counts, 3)),
                    (format!("SHORTEST 2 GROUPS (a){arrow}+(b)"), &walks, |counts| groups(counts, 2)),
                    (format!("SHORTEST 3 TRAIL (a){arrow}+(b)"), &trails, |counts| shortest(counts, 3)),
                    (format!("SHORTEST 2 TRAIL GROUPS (a){arrow}+(b)"), &trails, |counts| groups(counts, 2)),
                    (format!("ALL TRAIL (a){arrow}+(b)"), &trails, |counts| groups(counts, usize::MAX)),
                ];
                for (pattern, reference, keep) in cases {
                    let expected: BTreeMap<usize, Vec<usize>> = reference
                        .iter()
                        .map(|(node, counts)| (*node, keep(counts)))
                        .collect();
                    prop_assert_eq!(lengths(&db, &pattern, from), expected, "{} from {}", pattern, from);
                }
            }
        }
    }
}

#[test]
fn a_selector_keeps_paths_per_pair_of_end_nodes() {
    let db = graph(4, &[(0, 1), (1, 3), (0, 2), (2, 3), (0, 3)]);
    let rows = run(
        &db,
        "MATCH p = ALL SHORTEST (a:N)-[:R]->+(b:N) RETURN a.id, b.id, length(p) ORDER BY a.id, b.id",
    );
    let expected: Vec<Vec<Value>> = [(0, 1, 1), (0, 2, 1), (0, 3, 1), (1, 3, 1), (2, 3, 1)]
        .into_iter()
        .map(|(a, b, length)| vec![int(a), int(b), int(length)])
        .collect();
    assert_eq!(rows, expected);
}

fn ladder() -> Database {
    let db = db();
    run(
        &db,
        "INSERT (a:N {k: 1})-[:R {w: 1}]->(b:N {k: 2})-[:R {w: 1}]->(c:N {k: 3})-[:R {w: 1}]->(d:N {k: 4}),
                (a)-[:R {w: 9}]->(e:N {k: 5})-[:R {w: 9}]->(d)",
    );
    db
}

fn ks(db: &Database, query: &str) -> Vec<Value> {
    run(db, query)
        .into_iter()
        .map(|mut row| row.remove(0))
        .collect()
}

#[test]
fn cypher_shortest_path_functions_choose_the_shortest_paths() {
    let db = ladder();
    assert_eq!(
        ks(
            &db,
            "MATCH (a:N {k: 1}), (d:N {k: 4}) MATCH p = shortestPath((a)-[:R*]->(d)) RETURN [n IN nodes(p) | n.k]"
        ),
        [list([int(1), int(5), int(4)])]
    );
    assert_eq!(
        ks(
            &db,
            "MATCH p = allShortestPaths((a:N {k: 2})-[*]-(e:N {k: 5})) RETURN length(p)"
        ),
        [int(2)]
    );
}

#[test]
fn a_cypher_shortest_path_meets_its_where_before_it_is_chosen() {
    let db = ladder();
    let query = "MATCH p = shortestPath((a:N {k: 1})-[*]->(d:N {k: 4})) WHERE all(r IN relationships(p) WHERE r.w < 5) RETURN length(p)";
    assert_eq!(ks(&db, query), [int(3)]);
    let query = "MATCH p = ANY SHORTEST (a:N {k: 1})-[:R]->+(d:N {k: 4}) WHERE all(r IN relationships(p) WHERE r.w < 5) RETURN length(p)";
    assert!(run(&db, query).is_empty());
}

#[test]
fn an_optional_path_search_without_a_path_pads_with_null() {
    let db = ladder();
    let rows = run(
        &db,
        "MATCH (x:N {k: 4}) OPTIONAL MATCH p = ANY SHORTEST (x)-[:R]->+(y:N {k: 1}) RETURN x.k, p",
    );
    assert_eq!(rows, [vec![int(4), Value::Null]]);
}

#[test]
fn a_quantified_path_pattern_works_as_a_predicate_and_in_a_comprehension() {
    let db = ladder();
    assert_eq!(
        ks(&db, "MATCH (n:N) WHERE (n)-[:R]->{3}(:N {k: 4}) RETURN n.k"),
        [int(1)]
    );
    assert_eq!(
        ks(
            &db,
            "MATCH (n:N {k: 1}) RETURN [(n)((x)-[r:R]->(y) WHERE r.w = 1){2}(m) | m.k]"
        ),
        [list([int(3)])]
    );
}

#[test]
fn a_search_reads_bound_end_nodes_and_parameters() {
    let db = ladder();
    let parameters = Parameters::from([("from", 2i64), ("to", 4i64)]);
    let query =
        "MATCH (b:N {k: $to}) MATCH p = ANY SHORTEST (a:N {k: $from})-[:R]->+(b) RETURN length(p)";
    let rows = db.execute(query, &parameters).unwrap();
    assert_eq!(rows.rows(), [vec![int(2)]]);
}

#[test]
fn malformed_path_searches_fail_before_reading() {
    let db = ladder();
    for query in [
        "MATCH p = shortestPath((a)-[:R]->(b)-[:R]->(c)) RETURN p",
        "MATCH p = shortestPath((a)-[:R*2..]->(b)) RETURN p",
        "MATCH (m), p = ANY SHORTEST (a)-[:R]->(m)-[:R]->+(b) RETURN p",
        "MATCH p = ANY SHORTEST (a)-[:R]->(m)-[:R]->+(b {k: m.k}) RETURN p",
        "MATCH p = ANY SHORTEST (a)-[:R]->(m)((x)-[:R]->(y) WHERE y.k > m.k)+(b) RETURN p",
        "MATCH (a)((x)-[:R]->(x))+(b) RETURN a",
        "INSERT (a)((x)-[:R]->(y)){2}(b)",
        "MATCH ACYCLIC (a)-[:R]->(b) CREATE p = ANY SHORTEST (a)-[:R]->(b)",
    ] {
        assert_eq!(
            error_kind(&db, query),
            rhizodb::ErrorKind::InvalidPathPattern,
            "{query}"
        );
    }
}

#[test]
fn a_selective_search_shares_its_end_nodes_with_other_parts() {
    let db = ladder();
    let rows = run(
        &db,
        "MATCH (a:N {k: 1})-[:R]->(m), p = ANY SHORTEST (m)-[:R]->+(d:N {k: 4}) RETURN m.k, length(p) ORDER BY m.k",
    );
    assert_eq!(rows, [vec![int(2), int(2)], vec![int(5), int(1)]]);
}
