use std::cell::Cell;

use crate::Database;
use crate::Error;
use crate::ErrorKind;
use crate::OpenOptions;
use crate::Parameters;
use crate::QueryResult;
use crate::Value;
use crate::test_scale;

thread_local! {
    static BATCH: Cell<bool> = const { Cell::new(false) };
    static PRODUCED: Cell<usize> = const { Cell::new(0) };
}

pub(crate) fn batch_only() -> bool {
    BATCH.with(Cell::get)
}

pub(crate) fn produced() {
    PRODUCED.with(|produced| produced.set(produced.get() + 1));
}

type Outcome = Result<Vec<String>, ErrorKind>;

fn outcome(result: Result<QueryResult, Error>) -> Outcome {
    result
        .map(|result| result.rows().iter().map(|row| format!("{row:?}")).collect())
        .map_err(|error| error.kind())
}

fn counted(batch: bool, run: impl FnOnce() -> Result<QueryResult, Error>) -> (Outcome, usize) {
    BATCH.with(|flag| flag.set(batch));
    let before = PRODUCED.with(Cell::get);
    let result = outcome(run());
    let produced = PRODUCED.with(Cell::get) - before;
    BATCH.with(|flag| flag.set(false));
    (result, produced)
}

fn none() -> Parameters {
    Parameters::new()
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound.max(1)
    }

    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.below(items.len() as u64) as usize]
    }
}

fn graph_writes(rng: &mut Rng) -> Vec<String> {
    let nodes = 20 + rng.below(40);
    let mut writes = vec![format!(
        "UNWIND range(0, {}) AS i CREATE (n {{k: i, a: i % 5, b: CASE i % 4 WHEN 0 THEN 'ab' \
         WHEN 1 THEN 3 WHEN 2 THEN 'b' ELSE null END}})",
        nodes - 1
    )];
    for label in ["A", "B"] {
        writes.push(format!(
            "MATCH (n) WHERE n.k % {} = {} SET n:{label}",
            2 + rng.below(3),
            rng.below(2)
        ));
    }
    for rel in ["R", "S"] {
        let pairs: Vec<String> = (0..nodes + rng.below(nodes))
            .map(|_| format!("[{}, {}]", rng.below(nodes), rng.below(nodes)))
            .collect();
        writes.push(format!(
            "UNWIND [{}] AS p MATCH (a {{k: p[0]}}), (b {{k: p[1]}}) CREATE (a)-[:{rel} {{w: p[0] % 3}}]->(b)",
            pairs.join(", ")
        ));
    }
    writes
}

fn build(writes: &[String], options: OpenOptions) -> Database {
    let db = options.in_memory().unwrap();
    for write in writes {
        db.execute(write, &none()).unwrap();
    }
    db
}

const CONDITIONS: &[&str] = &[
    "a.k > 3",
    "a.a = b.a",
    "a.b STARTS WITH 'a'",
    "a:A",
    "b.k IN [1, 2, 3, 5, 8]",
    "a.k % 3 = 0 OR b IS NULL",
    "1 / (a.k - 4) > 0",
    "a.b + 1 > 2",
    "EXISTS { MATCH (a)-[:S]->() }",
    "COUNT { MATCH (a)-->() } > 1",
];

const ITEMS: &[&str] = &[
    "a, b",
    "a.k, b.k",
    "a.k AS k, b.a AS ba",
    "a.k / (b.k - 3) AS q",
    "a.b + 1 AS c",
    "b.k AS k",
    "a.k, toInteger(a.b) AS t",
];

const AGGREGATES: &[&str] = &[
    "count(*)",
    "a.a AS g, count(*), collect(b.k)",
    "sum(a.k), avg(b.k)",
    "a.a AS g, max(a.k / (b.k - 2))",
    "count(DISTINCT b), min(a.b)",
];

fn query(rng: &mut Rng) -> String {
    let mut parts = Vec::new();
    match rng.below(5) {
        0 => parts.push("MATCH (a)-[:R]->(b)".to_owned()),
        1 => parts.push("MATCH (a:A)-[:R]->(b)-[:S]->(c)".to_owned()),
        2 => parts.push("MATCH (a) OPTIONAL MATCH (a)-[:S]->(b)".to_owned()),
        3 => parts.push(format!(
            "UNWIND range(0, {}) AS i MATCH (a {{k: i}})-->(b)",
            rng.below(30)
        )),
        _ => parts.push("MATCH (a:B) MATCH (b) WHERE b.k = a.k + 1".to_owned()),
    }
    if rng.below(2) == 0 {
        let joiner = if parts[0].contains("WHERE") {
            "AND"
        } else {
            "WHERE"
        };
        parts.push(format!("{joiner} ({})", rng.pick(CONDITIONS)));
    }
    for _ in 0..rng.below(3) {
        let middle = match rng.below(8) {
            0 => "WITH a, b".to_owned(),
            1 => "WITH DISTINCT a, b".to_owned(),
            2 => "UNWIND [1, 2] AS x WITH a, b".to_owned(),
            3 => format!("WITH a, b WHERE {}", rng.pick(CONDITIONS)),
            4 => format!("WITH a, b ORDER BY a.k DESC, b.k LIMIT {}", rng.below(8)),
            5 => "OPTIONAL MATCH (b)-[:S]->(c) WITH a, b".to_owned(),
            6 => format!(
                "WITH a, b LIMIT {} WHERE {}",
                rng.below(10),
                rng.pick(CONDITIONS)
            ),
            _ => format!("WITH a, b SKIP {} LIMIT {}", rng.below(3), rng.below(10)),
        };
        parts.push(middle);
    }
    if rng.below(3) == 0 {
        parts.push(format!("RETURN {}", rng.pick(AGGREGATES)));
        return parts.join(" ");
    }
    let distinct = if rng.below(4) == 0 { "DISTINCT " } else { "" };
    parts.push(format!("RETURN {distinct}{}", rng.pick(ITEMS)));
    if distinct.is_empty() && rng.below(3) == 0 {
        parts.push("ORDER BY a.k, b.k DESC".to_owned());
    }
    if rng.below(3) == 0 {
        parts.push(format!("SKIP {}", rng.below(4)));
    }
    if rng.below(4) != 0 {
        parts.push(format!("LIMIT {}", rng.below(12)));
    }
    parts.join(" ")
}

#[test]
fn streamed_queries_match_clauses_applied_in_turn() {
    let mut rng = Rng(test_scale::seed());
    let (mut found, mut failed) = (0, 0);
    for _ in 0..test_scale::cases(12) {
        let writes = graph_writes(&mut rng);
        let serial = build(&writes, OpenOptions::new().threads(1));
        let split = build(&writes, OpenOptions::new().threads(4).parallel_threshold(2));
        for _ in 0..30 {
            let text = query(&mut rng);
            let (in_turn, _) = counted(true, || serial.execute(&text, &none()));
            let (streamed, _) = counted(false, || serial.execute(&text, &none()));
            let (parallel, _) = counted(false, || split.execute(&text, &none()));
            if matches!(
                in_turn,
                Err(ErrorKind::Syntax(_) | ErrorKind::UndefinedVariable)
            ) {
                panic!("{text} does not bind: {in_turn:?}");
            }
            assert_eq!(streamed, in_turn, "{text}\n{writes:#?}");
            match &in_turn {
                Ok(_) => found += 1,
                Err(_) => failed += 1,
            }
            assert_eq!(parallel, in_turn, "{text} split\n{writes:#?}");
        }
    }
    assert!(
        found > 0 && failed > 0,
        "{found} queries found rows, {failed} failed"
    );
}

#[test]
fn streamed_writes_match_clauses_applied_in_turn() {
    let mut rng = Rng(test_scale::seed());
    let writes_around_limits = [
        "MATCH (a)-[:R]->(b) WITH a, b LIMIT $n SET a.t = coalesce(a.t, 0) + 1 RETURN count(*)",
        "MATCH (a)-[:R]->(b) SET a.t = coalesce(a.t, 0) + 1 WITH a, b RETURN a.k, a.t LIMIT $n",
        "MATCH (a) SET a.t = a.k WITH a MATCH (a)-[:R]->(b) RETURN b.k, b.t LIMIT $n",
        "MATCH (a)-[:R]->(b) DETACH DELETE b WITH a RETURN a.k LIMIT $n",
        "MATCH (a)-[:R]->(b) DELETE b WITH a, b RETURN b.k LIMIT $n",
        "UNWIND range(1, 5) AS i CREATE (c {k: 100 + i}) WITH c MATCH (a) RETURN a.k LIMIT $n",
    ];
    for _ in 0..test_scale::cases(8) {
        let writes = graph_writes(&mut rng);
        for text in writes_around_limits {
            let parameters = Parameters::new().with("n", rng.below(6) as i64);
            let mut outcomes = Vec::new();
            for batch in [true, false] {
                let db = build(&writes, OpenOptions::new().threads(1));
                let (rows, _) = counted(batch, || db.execute(text, &parameters));
                let graph = outcome(db.execute(
                    "MATCH (n) OPTIONAL MATCH (n)-[r]->(m) RETURN n.k, n.t, m.k ORDER BY n.k, m.k",
                    &none(),
                ));
                outcomes.push((rows, graph));
            }
            assert_eq!(outcomes[1], outcomes[0], "{text} {parameters:?}");
        }
    }
}

fn chain(edges: i64) -> Database {
    let db = Database::in_memory().unwrap();
    db.execute(
        &format!(
            "UNWIND range(0, {edges}) AS i CREATE (:N {{k: i}}) WITH count(*) AS one \
             MATCH (n:N) WITH n ORDER BY n.k WITH collect(n) AS ns \
             UNWIND range(0, size(ns) - 2) AS i WITH ns[i] AS a, ns[i + 1] AS b \
             CREATE (a)-[:R]->(b)"
        ),
        &none(),
    )
    .unwrap();
    db
}

fn matches_produced(db: &Database, text: &str) -> (usize, usize) {
    let (in_turn, all) = counted(true, || db.execute(text, &none()));
    let (streamed, read) = counted(false, || db.execute(text, &none()));
    assert_eq!(streamed, in_turn, "{text}");
    (read, all)
}

#[test]
fn a_limit_stops_the_match_once_it_has_its_rows() {
    let db = chain(500);
    for (text, most) in [
        ("MATCH (a)-[:R]->(b) RETURN a, b LIMIT 3", 3),
        ("MATCH (a)-[:R]->(b)-[:R]->(c) RETURN a.k, c.k LIMIT 10", 10),
        (
            "MATCH (a)-[:R]->(b) WHERE b.k > 100 RETURN a.k SKIP 5 LIMIT 5",
            110,
        ),
        ("MATCH (a)-[:R]->(b) RETURN DISTINCT a LIMIT 3", 3),
        ("MATCH (a)-[:R]->(b) WITH a, b LIMIT 4 RETURN a.k, b.k", 4),
        ("MATCH (a:N) MATCH (a)-[:R]->(b) RETURN b.k LIMIT 2", 4),
        (
            "MATCH (a:N) OPTIONAL MATCH (a)<-[:R]-(b) RETURN a.k, b.k LIMIT 2",
            4,
        ),
        ("MATCH (a)-[:R]->(b) RETURN a.k LIMIT 0", 1),
    ] {
        let (read, all) = matches_produced(&db, text);
        assert!(
            read <= most,
            "{text} produced {read} matches, more than {most}"
        );
        assert!(all > 100, "{text} produced only {all} matches in turn");
    }
}

#[test]
fn a_limit_reads_every_match_when_a_later_row_could_fail() {
    let db = chain(500);
    for text in [
        "MATCH (a)-[:R]->(b) RETURN 10 / (b.k - 1000) AS q LIMIT 3",
        "MATCH (a)-[:R]->(b) WHERE 10 / (b.k - 1000) < 0 RETURN a.k LIMIT 3",
        "MATCH (a)-[:R]->(b) RETURN a.k + b.k AS s LIMIT 0",
    ] {
        let (read, all) = matches_produced(&db, text);
        assert_eq!(read, all, "{text}");
    }
    let (error, _) = counted(false, || {
        db.execute(
            "MATCH (a)-[:R]->(b) RETURN 10 / (b.k - 400) AS q LIMIT 3",
            &none(),
        )
    });
    assert_eq!(error, Err(ErrorKind::DivisionByZero));
}

#[test]
fn an_exists_subquery_stops_at_its_first_row() {
    let db = chain(500);
    for text in [
        "RETURN EXISTS { MATCH (a)-[:R]->(b) RETURN a } AS found",
        "MATCH (n:N {k: 0}) RETURN EXISTS { MATCH (a)-[:R]->(b) WHERE b.k > n.k RETURN b } AS found",
        "MATCH (n:N {k: 0}) RETURN EXISTS { MATCH (a)-[:R]->(b) WITH b AS c WHERE c.k > n.k \
         RETURN c } AS found",
    ] {
        let (read, all) = matches_produced(&db, text);
        assert!(read <= 2, "{text} produced {read} matches");
        assert!(all > 100, "{text} produced only {all} matches in turn");
    }
    let (read, all) = matches_produced(
        &db,
        "RETURN EXISTS { MATCH (a)-[:R]->(b) WHERE 1 / (b.k - 1000) < 0 RETURN a } AS found",
    );
    assert_eq!(read, all);
}

#[test]
fn a_write_before_a_limit_applies_to_every_row() {
    let db = chain(50);
    let mut tx = db.begin_write().unwrap();
    let returned = tx
        .execute(
            "MATCH (a)-[:R]->(b) SET b.seen = true WITH b RETURN b.k LIMIT 1",
            &none(),
        )
        .unwrap();
    assert_eq!(returned.rows().len(), 1);
    let seen = tx
        .execute("MATCH (n) WHERE n.seen RETURN count(*)", &none())
        .unwrap();
    assert_eq!(seen.rows()[0], vec![Value::Integer(50)]);
}

const ORDER_KEYS: &[&str] = &[
    "a.a",
    "a.a DESC, b.k",
    "b.a, a.k DESC",
    "a.b",
    "a.b DESC",
    "b.b ASC NULLS FIRST, a.k",
    "a.k % 3, b.a DESC NULLS LAST",
    "1 / (a.k - 4)",
    "toInteger(a.b), a.k",
    "EXISTS { MATCH (a)-[:S]->() }, b.k",
];

const SORTED_ITEMS: &[&str] = &["a.k, b.k", "a.a AS g, b.k", "a.k / (b.k - 3) AS q", "a, b"];

fn sorted_query(rng: &mut Rng) -> (String, String, usize, Option<usize>) {
    let mut parts = vec![
        match rng.below(3) {
            0 => "MATCH (a)-[:R]->(b)",
            1 => "MATCH (a) OPTIONAL MATCH (a)-[:S]->(b)",
            _ => "UNWIND range(0, 30) AS i MATCH (a {k: i}) MATCH (b) WHERE b.k = a.k % 7",
        }
        .to_owned(),
    ];
    if rng.below(3) == 0 {
        parts.push(format!("WITH a, b WHERE {}", rng.pick(CONDITIONS)));
    }
    let distinct = if rng.below(4) == 0 { "DISTINCT " } else { "" };
    let items = if distinct.is_empty() {
        rng.pick(SORTED_ITEMS)
    } else {
        "a.a AS g, b.a AS h"
    };
    parts.push(format!("RETURN {distinct}{items}"));
    let keys = if distinct.is_empty() {
        rng.pick(ORDER_KEYS)
    } else {
        "g DESC, h"
    };
    parts.push(format!("ORDER BY {keys}"));
    let plain = parts.join(" ");
    let skip = rng.below(4) as usize;
    let limit = (rng.below(4) != 0).then(|| rng.below(12) as usize);
    if skip > 0 {
        parts.push(format!("SKIP {skip}"));
    }
    if let Some(limit) = limit {
        parts.push(format!("LIMIT {limit}"));
    }
    (parts.join(" "), plain, skip, limit)
}

#[test]
fn an_order_by_with_a_limit_keeps_the_rows_and_the_first_error_of_a_full_sort() {
    let mut rng = Rng(test_scale::seed());
    let (mut found, mut failed) = (0, 0);
    for _ in 0..test_scale::cases(12) {
        let writes = graph_writes(&mut rng);
        let serial = build(&writes, OpenOptions::new().threads(1));
        let split = build(&writes, OpenOptions::new().threads(4).parallel_threshold(2));
        for _ in 0..30 {
            let (text, plain, skip, limit) = sorted_query(&mut rng);
            let (in_turn, _) = counted(true, || serial.execute(&text, &none()));
            let (streamed, _) = counted(false, || serial.execute(&text, &none()));
            let (parallel, _) = counted(false, || split.execute(&text, &none()));
            let (sorted, _) = counted(false, || serial.execute(&plain, &none()));
            let sliced = sorted.map(|rows| {
                rows.into_iter()
                    .skip(skip)
                    .take(limit.unwrap_or(usize::MAX))
                    .collect::<Vec<_>>()
            });
            assert_eq!(in_turn, sliced, "{text}\n{writes:#?}");
            assert_eq!(streamed, in_turn, "{text}\n{writes:#?}");
            assert_eq!(parallel, in_turn, "{text} split\n{writes:#?}");
            match &in_turn {
                Ok(_) => found += 1,
                Err(_) => failed += 1,
            }
        }
    }
    assert!(
        found > 0 && failed > 0,
        "{found} queries found rows, {failed} failed"
    );
}

#[test]
fn an_order_by_with_a_limit_raises_an_error_of_its_last_row() {
    let db = chain(500);
    for (text, error) in [
        (
            "MATCH (a)-[:R]->(b) RETURN b.k ORDER BY 1 / (b.k - 480) LIMIT 2",
            ErrorKind::DivisionByZero,
        ),
        (
            "MATCH (a)-[:R]->(b) RETURN b.k ORDER BY CASE b.k WHEN 480 THEN b.k + true END LIMIT 2",
            ErrorKind::TypeMismatch,
        ),
        (
            "MATCH (a)-[:R]->(b) RETURN 1 / (b.k - 480) AS q \
             ORDER BY CASE b.k WHEN 5 THEN b.k + true END LIMIT 2",
            ErrorKind::DivisionByZero,
        ),
        (
            "MATCH (a)-[:R]->(b) WITH b ORDER BY b.k SKIP 1 LIMIT 1 WHERE 1 / (b.k - 2) > 0 \
             RETURN b.k",
            ErrorKind::DivisionByZero,
        ),
    ] {
        let (in_turn, _) = counted(true, || db.execute(text, &none()));
        let (streamed, _) = counted(false, || db.execute(text, &none()));
        assert_eq!(in_turn, Err(error), "{text}");
        assert_eq!(streamed, in_turn, "{text}");
    }
}
