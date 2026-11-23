mod support;

use std::time::Duration;

use rhizodb::Database;
use rhizodb::ErrorKind;
use rhizodb::IndexTarget;
use rhizodb::Parameters;
use rhizodb::SyntaxErrorKind;
use rhizodb::Value;
use support::*;

const CHILD_LIMIT: Duration = Duration::from_secs(300);

/// The deepest list a host can build, clone, and drop on a 2 MiB stack with room to spare, in
/// the profile the test runs in.
const HOST_SAFE_DEPTH: usize = if cfg!(debug_assertions) { 2_000 } else { 8_000 };

fn nested_list(depth: usize) -> Value {
    let mut value = Value::Integer(1);
    for _ in 0..depth {
        value = Value::List(vec![value]);
    }
    value
}

fn parameter(name: &str, value: Value) -> Parameters {
    Parameters::from([(name.to_owned(), value)])
}

#[test]
#[ignore = "bug: converting a parameter recurses once per nesting level, so a nested list the \
            host builds, clones, and drops on a 2 MiB stack aborts the process with a stack \
            overflow inside execute instead of failing with an error"]
fn a_deeply_nested_parameter_fails_or_answers_without_a_stack_overflow() {
    let test = "a_deeply_nested_parameter_fails_or_answers_without_a_stack_overflow";
    if !isolated(test, CHILD_LIMIT) {
        return;
    }
    let db = db();
    on_small_stack(|| {
        let value = nested_list(HOST_SAFE_DEPTH);
        drop(value.clone());
        for query in [
            "RETURN $p IS NULL",
            "RETURN size($p)",
            "INSERT (:N {p: $p})",
        ] {
            match db.execute(query, &parameter("p", value.clone())) {
                Ok(_) => {}
                Err(error) => assert!(
                    matches!(
                        error.kind(),
                        ErrorKind::InvalidPropertyType | ErrorKind::NumberOutOfRange
                    ),
                    "{query}: {error}"
                ),
            }
        }
    });
}

#[test]
fn a_parameter_nested_within_the_stored_limit_round_trips_and_one_past_it_is_refused() {
    let db = db();
    on_small_stack(|| {
        let stored = nested_list(64);
        db.execute("INSERT (:N {p: $p})", &parameter("p", stored.clone()))
            .unwrap();
        assert_eq!(
            db.execute("MATCH (n:N) RETURN n.p = $p", &parameter("p", stored))
                .unwrap()
                .rows(),
            [vec![boolean(true)]]
        );
        let error = db
            .execute("INSERT (:N {p: $p})", &parameter("p", nested_list(65)))
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidPropertyType, "{error}");
        assert_eq!(single(&db, "MATCH (n:N) RETURN count(*)"), int(1));
    });
}

#[test]
fn deeply_nested_query_text_is_a_syntax_error_on_a_small_stack() {
    let db = db();
    let depth = 100_000;
    let texts = [
        format!("RETURN {}1{}", "(".repeat(depth), ")".repeat(depth)),
        format!("RETURN {}1{}", "[".repeat(depth), "]".repeat(depth)),
        format!("RETURN {}true", "NOT ".repeat(depth)),
        format!("RETURN 1{}", " + 1".repeat(depth)),
        format!("MATCH (n) WHERE {}true RETURN n", "NOT ".repeat(depth)),
        format!("RETURN {}", "exists { MATCH (a) WHERE ".repeat(depth)),
    ];
    on_small_stack(|| {
        for text in &texts {
            let error = db.execute(text, &Parameters::new()).unwrap_err();
            assert_eq!(
                error.kind(),
                ErrorKind::Syntax(SyntaxErrorKind::NestingTooDeep),
                "{}",
                &text[..40]
            );
        }
    });
}

#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "bug: in a debug build, a chain of 92 NOT or + operators, which the parser accepts \
              below its nesting limit of 128, aborts the process with a stack overflow on a 2 MiB \
              stack, so the limit leaves no headroom for debug builds of hosts"
)]
fn nesting_just_inside_the_parser_limit_runs_on_a_small_stack() {
    let test = "nesting_just_inside_the_parser_limit_runs_on_a_small_stack";
    if !isolated(test, CHILD_LIMIT) {
        return;
    }
    let db = db();
    let texts: Vec<String> = [40, 80, 100, 120, 126]
        .into_iter()
        .flat_map(|depth: usize| {
            [
                format!("RETURN {}1{}", "(".repeat(depth), ")".repeat(depth)),
                format!("RETURN size({}1{})", "[".repeat(depth), "]".repeat(depth)),
                format!("RETURN {}true", "NOT ".repeat(depth)),
                format!("RETURN 1{}", " + 1".repeat(depth)),
                format!("MATCH (n) WHERE {}true RETURN n", "NOT ".repeat(depth)),
                format!(
                    "RETURN {}1{}",
                    "CASE WHEN true THEN ".repeat(depth),
                    " END".repeat(depth)
                ),
                format!("RETURN {}1{}", "abs(".repeat(depth), ")".repeat(depth)),
            ]
        })
        .collect();
    on_small_stack(|| {
        for text in &texts {
            if let Err(error) = db.execute(text, &Parameters::new()) {
                assert_eq!(
                    error.kind(),
                    ErrorKind::Syntax(SyntaxErrorKind::NestingTooDeep),
                    "{}: {error}",
                    &text[..40]
                );
            }
        }
    });
}

fn chain(db: &Database, hops: usize) {
    let mut text = String::from("INSERT (:C {i: 0})");
    for i in 1..=hops {
        text.push_str(&format!("-[:NEXT]->(:C {{i: {i}}})"));
    }
    run(db, &text);
}

#[test]
fn variable_length_paths_over_a_chain_of_a_thousand_answer_on_a_small_stack() {
    let db = db();
    chain(&db, 1_000);
    on_small_stack(|| {
        assert_eq!(
            single(&db, "MATCH (a:C {i: 0})-[:NEXT*]->(b) RETURN count(b)"),
            int(1_000)
        );
        assert_eq!(
            single(
                &db,
                "MATCH p = (a:C {i: 1000})<-[:NEXT*1..5000]-(b) RETURN max(length(p))"
            ),
            int(1_000)
        );
        assert_eq!(
            single(
                &db,
                "MATCH p = (a:C {i: 500})-[*]-(b) RETURN max(length(p))"
            ),
            int(500)
        );
    });
}

#[test]
#[ignore = "bug: variable-length expansion recurses once per hop, so a match over a chain of 20000 \
            relationships aborts the process with a stack overflow on a 2 MiB stack, and on a \
            chain of 2000 in a debug build"]
fn a_variable_length_path_over_a_long_chain_answers_without_a_stack_overflow() {
    let test = "a_variable_length_path_over_a_long_chain_answers_without_a_stack_overflow";
    if !isolated(test, CHILD_LIMIT) {
        return;
    }
    let db = db();
    chain(&db, 20_000);
    on_small_stack(|| {
        assert_eq!(
            single(&db, "MATCH (a:C {i: 0})-[:NEXT*]->(b) RETURN count(b)"),
            int(20_000)
        );
    });
}

#[test]
fn a_pattern_of_twenty_thousand_hops_and_a_union_of_two_thousand_parts_run() {
    let db = db();
    chain(&db, 30);
    on_small_stack(|| {
        let text = format!("MATCH (a){} RETURN count(*)", "-->()".repeat(20_000));
        assert_eq!(single(&db, &text), int(0));
        let text = format!("MATCH (a:C {{i: 0}}){} RETURN count(*)", "-->()".repeat(30));
        assert_eq!(single(&db, &text), int(1));
        let text = vec!["RETURN 1 AS x"; 2_000].join(" UNION ALL ");
        assert_eq!(run(&db, &text).len(), 2_000);
    });
}

#[test]
fn range_refuses_to_build_a_list_past_its_limit_however_little_is_read() {
    let db = db();
    for text in [
        "UNWIND range(1, 9223372036854775807) AS i RETURN i LIMIT 1",
        "RETURN size(range(-9223372036854775808, 9223372036854775807))",
        "RETURN size(range(1, 4000000000))",
    ] {
        assert_eq!(error_kind(&db, text), ErrorKind::NumberOutOfRange, "{text}");
    }
    assert_eq!(
        single(&db, "RETURN size(range(1, 1000000))"),
        int(1_000_000)
    );
}

#[test]
fn skip_and_limit_at_the_integer_maximum_neither_overflow_nor_allocate() {
    let db = db();
    assert_eq!(
        run(
            &db,
            "UNWIND [1, 2] AS x RETURN x SKIP 9223372036854775807 LIMIT 9223372036854775807"
        ),
        Vec::<Vec<Value>>::new()
    );
    assert_eq!(
        run(&db, "UNWIND [1, 2] AS x RETURN x LIMIT 9223372036854775807").len(),
        2
    );
}

#[test]
fn a_property_of_eight_megabytes_and_a_list_of_half_a_million_survive_reopening() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph.redb");
    let text = "ü".repeat(4 << 20);
    let items: Vec<Value> = (0..500_000).map(Value::Integer).collect();
    {
        let db = Database::open(&path).unwrap();
        db.create_keyword_index("words", IndexTarget::Label("Big"), &["s"])
            .unwrap();
        db.execute(
            "INSERT (:Big {s: $s, l: $l})",
            &Parameters::from([
                ("s".to_owned(), Value::String(text.clone())),
                ("l".to_owned(), Value::List(items)),
            ]),
        )
        .unwrap();
    }
    let db = Database::open(&path).unwrap();
    assert_eq!(
        run(
            &db,
            "MATCH (n:Big) RETURN size(n.s), size(n.l), n.l[0], n.l[-1]"
        ),
        [vec![int(4 << 20), int(500_000), int(0), int(499_999)]]
    );
    assert_eq!(
        db.execute(
            "MATCH (n:Big) RETURN n.s = $s",
            &parameter("s", Value::String(text))
        )
        .unwrap()
        .rows(),
        [vec![boolean(true)]]
    );
}

#[test]
fn names_of_a_megabyte_are_stored_and_read_back() {
    let db = db();
    let name = "k".repeat(1 << 20);
    run(
        &db,
        &format!("INSERT (:`{name}` {{`{name}`: 1}})-[:`{name}`]->(:Small)"),
    );
    assert_eq!(
        run(
            &db,
            "MATCH (a)-[r]->(b) RETURN size(labels(a)[0]), size(keys(a)[0]), size(type(r))"
        ),
        [vec![int(1 << 20), int(1 << 20), int(1 << 20)]]
    );
}

#[test]
fn a_write_transaction_of_many_statements_and_nodes_commits_whole_or_rolls_back_whole() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph.redb");
    let batches = cases(2).min(50) as i64;
    let per_batch = 10_000;
    {
        let db = Database::open(&path).unwrap();
        db.create_property_index("by_i", IndexTarget::Label("B"), "i")
            .unwrap();
        db.create_keyword_index("words", IndexTarget::Label("B"), &["s"])
            .unwrap();
        for commit in [false, true] {
            let mut tx = db.begin_write().unwrap();
            for batch in 0..batches {
                tx.execute(
                    "UNWIND range($from, $from + $n - 1) AS i \
                     INSERT (:B {i: i, s: 'word ' + toString(i % 97)})",
                    &Parameters::from([
                        ("from".to_owned(), int(batch * per_batch)),
                        ("n".to_owned(), int(per_batch)),
                    ]),
                )
                .unwrap();
            }
            if commit {
                tx.commit().unwrap();
            } else {
                tx.rollback().unwrap();
                assert_eq!(single(&db, "MATCH (n:B) RETURN count(*)"), int(0));
            }
        }
    }
    let db = Database::open(&path).unwrap();
    let total = batches * per_batch;
    assert_eq!(
        run(&db, "MATCH (n:B) RETURN count(*), sum(n.i)"),
        [vec![int(total), int(total * (total - 1) / 2)]]
    );
    assert_eq!(
        single(
            &db,
            &format!("MATCH (n:B {{i: {}}}) RETURN count(*)", total - 1)
        ),
        int(1)
    );
    assert_eq!(
        single(
            &db,
            "CALL db.index.keyword.queryNodes('words', 'word', 1000000) YIELD node \
             RETURN count(node)"
        ),
        int(total)
    );
}
