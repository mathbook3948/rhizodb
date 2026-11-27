mod support;

use std::time::Duration;

use rhizodb::Database;
use rhizodb::ErrorKind;
use rhizodb::IndexTarget;
use rhizodb::OpenOptions;
use rhizodb::Parameters;
use rhizodb::Procedure;
use rhizodb::SyntaxErrorKind;
use rhizodb::Value;
use rhizodb::ValueType;
use support::*;

const CHILD_LIMIT: Duration = Duration::from_secs(300);

/// How deep a value in a query may nest lists and maps.
const MAX_VALUE_NESTING: usize = 128;

/// The deepest list a host can build, clone, and drop on a 2 MiB stack with room to spare, in
/// the profile the test runs in.
const HOST_SAFE_DEPTH: usize = if cfg!(debug_assertions) { 2_000 } else { 8_000 };

/// The stack the queries nested to the parser's limit must fit in. A release build gets half of
/// the 1 MiB a query may use, because its frames grow or shrink with what the compiler inlines on
/// each target, so a query that fits with no room to spare on one target overflows on another.
const NESTING_STACK: usize = if cfg!(debug_assertions) {
    1 << 20
} else {
    1 << 19
};

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
fn a_parameter_nested_past_the_value_limit_fails_without_recursing_over_it() {
    let test = "a_parameter_nested_past_the_value_limit_fails_without_recursing_over_it";
    if !isolated(test, CHILD_LIMIT) {
        return;
    }
    let db = db();
    let deep = parameter("p", nested_list(HOST_SAFE_DEPTH));
    on_small_stack(|| {
        for query in [
            "RETURN $p IS NULL",
            "RETURN size($p)",
            "INSERT (:N {p: $p})",
        ] {
            let error = db.execute(query, &deep).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::NestingTooDeep, "{query}: {error}");
        }
    });
    assert_eq!(single(&db, "MATCH (n) RETURN count(*)"), int(0));
}

fn nested_value(depth: usize) -> Value {
    let mut value = Value::Integer(1);
    for level in 0..depth {
        value = if level % 2 == 0 {
            Value::List(vec![value])
        } else {
            Value::Map([("k".to_owned(), value)].into())
        };
    }
    value
}

#[test]
fn a_value_nested_to_the_limit_is_read_compared_grouped_and_returned_on_a_small_stack() {
    let db = db();
    on_small_stack(|| {
        let full = parameter("p", nested_value(MAX_VALUE_NESTING));
        assert_eq!(
            db.execute("RETURN $p = $p, $p < $p, $p IS NULL", &full)
                .unwrap()
                .rows(),
            [vec![boolean(true), Value::Null, boolean(false)]]
        );
        assert_eq!(
            db.execute("RETURN $p", &full).unwrap().rows(),
            [vec![nested_value(MAX_VALUE_NESTING)]]
        );
        let inner = parameter("p", nested_value(MAX_VALUE_NESTING - 1));
        for query in [
            "UNWIND [$p, $p] AS x RETURN DISTINCT x",
            "UNWIND [$p, $p] AS x RETURN x ORDER BY x",
            "UNWIND [$p, $p] AS x RETURN x, count(*)",
            "UNWIND [$p] AS x RETURN collect(x)[0]",
            "RETURN [$p][0]",
            "RETURN [x IN [$p] | x][0]",
            "RETURN ([] + $p)",
        ] {
            assert_eq!(
                db.execute(query, &inner).unwrap().rows()[0][0],
                nested_value(MAX_VALUE_NESTING - 1),
                "{query}"
            );
        }
        let error = db
            .execute(
                "RETURN $p",
                &parameter("p", nested_value(MAX_VALUE_NESTING + 1)),
            )
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::NestingTooDeep, "{error}");
    });
}

#[test]
fn a_query_that_builds_a_value_past_the_nesting_limit_fails() {
    let db = db();
    let inner = parameter("p", nested_list(MAX_VALUE_NESTING));
    on_small_stack(|| {
        for query in [
            "RETURN [$p]",
            "RETURN {k: $p}",
            "RETURN [x IN [1] | $p]",
            "UNWIND [1] AS x RETURN collect($p)",
            "RETURN [1] + {k: $p[0]}",
            "RETURN {k: $p[0]} + [1]",
        ] {
            let error = db.execute(query, &inner).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::NestingTooDeep, "{query}: {error}");
        }
        assert_eq!(
            db.execute("RETURN size([1] + $p), size($p + {k: 1})", &inner)
                .unwrap()
                .rows(),
            [vec![int(2), int(2)]]
        );
        let text = format!(
            "WITH 1 AS x {} RETURN x",
            "WITH [x] AS x ".repeat(MAX_VALUE_NESTING + 1)
        );
        assert_eq!(error_kind(&db, &text), ErrorKind::NestingTooDeep);
        let text = format!(
            "WITH 1 AS x {} RETURN size(x)",
            "WITH [x] AS x ".repeat(MAX_VALUE_NESTING)
        );
        assert_eq!(single(&db, &text), int(1));
        let text = format!(
            "UNWIND [1] AS x WITH collect(x) AS x {} RETURN size(x)",
            "UNWIND [1] AS y WITH collect(x) AS x ".repeat(MAX_VALUE_NESTING)
        );
        assert_eq!(error_kind(&db, &text), ErrorKind::NestingTooDeep);
    });
}

#[test]
fn a_procedure_result_nested_past_the_value_limit_fails() {
    let db = db();
    db.register_procedure(
        Procedure::new("test.deep", |args| {
            let depth = match args.first() {
                Some(Value::Integer(depth)) => *depth as usize,
                _ => 0,
            };
            Ok(vec![vec![nested_list(depth)]].into())
        })
        .parameter("depth", ValueType::Integer)
        .output("value"),
    )
    .unwrap();
    on_small_stack(|| {
        let query = |depth: usize| {
            db.execute(
                &format!("CALL test.deep({depth}) YIELD value RETURN size(value)"),
                &Parameters::new(),
            )
        };
        assert_eq!(query(MAX_VALUE_NESTING).unwrap().rows(), [vec![int(1)]]);
        let error = query(HOST_SAFE_DEPTH).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::NestingTooDeep, "{error}");
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
        format!("RETURN {}", "COUNT { MATCH (a) WHERE ".repeat(depth)),
        format!("RETURN {}", "VALUE { RETURN ".repeat(depth)),
        format!("RETURN {}1", "CAST(".repeat(depth)),
        format!("RETURN 1 IS :: {}INTEGER", "LIST<".repeat(depth)),
        format!("RETURN 1{}", " IS NOT TYPED INTEGER".repeat(depth)),
        "CALL { ".repeat(depth),
        format!("RETURN {}", "reduce(a = ".repeat(depth)),
        format!(
            "MATCH (n:{}A{}) RETURN n",
            "(".repeat(depth),
            ")".repeat(depth)
        ),
        format!("RETURN {}1{}", "{a: ".repeat(depth), "}".repeat(depth)),
        format!("RETURN null{}", "[0]".repeat(depth)),
        format!("RETURN {}1", "- ".repeat(depth)),
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
        let text = format!(
            "RETURN 'a' =~ '{}a{}'",
            "(".repeat(depth),
            ")".repeat(depth)
        );
        assert_eq!(error_kind(&db, &text), ErrorKind::InvalidRegex);
    });
}

#[test]
fn a_long_chain_of_next_statements_runs_on_a_small_stack() {
    let db = db();
    let text = format!("RETURN 0 AS a{}", " NEXT RETURN a + 1 AS a".repeat(20_000));
    on_small_stack(|| {
        assert_eq!(single(&db, &text), int(20_000));
    });
}

#[test]
fn a_node_pattern_with_thousands_of_properties_runs_on_a_small_stack() {
    let test = "a_node_pattern_with_thousands_of_properties_runs_on_a_small_stack";
    if !isolated(test, CHILD_LIMIT) {
        return;
    }
    let db = db();
    run(&db, "INSERT (:L {a0: 1}), (:L {a0: 1, a1: 1})");
    let entries: Vec<String> = (0..5_000).map(|key| format!("a{key}: 1")).collect();
    let entries = entries.join(", ");
    on_small_stack(|| {
        for text in [
            format!("MATCH (n:L {{{entries}}}) RETURN count(*)"),
            format!("MATCH (n {{{entries}}}) RETURN count(*)"),
        ] {
            assert_eq!(single(&db, &text), int(0));
        }
        assert_eq!(
            single(&db, "MATCH (n:L {a0: 1, a1: 1}) RETURN count(*)"),
            int(1)
        );
    });
}

#[test]
fn long_flat_chains_of_logic_comparisons_and_tests_run_on_a_small_stack() {
    let test = "long_flat_chains_of_logic_comparisons_and_tests_run_on_a_small_stack";
    if !isolated(test, CHILD_LIMIT) {
        return;
    }
    let db = db();
    run(&db, "INSERT (:L {a: 7}), (:L {a: 20000})");
    let ors: Vec<String> = (0..20_000).map(|value| format!("n.a = {value}")).collect();
    let tests: Vec<String> = (0..20_000).map(|value| value.to_string()).collect();
    let texts = [
        format!("MATCH (n:L) WHERE {} RETURN count(*)", ors.join(" OR ")),
        format!("MATCH (n) WHERE {} RETURN count(*)", ors.join(" OR ")),
        format!(
            "MATCH (n:L) RETURN sum(CASE n.a WHEN {} THEN 1 ELSE 0 END)",
            tests.join(", ")
        ),
        format!("RETURN toInteger(true{})", " AND true".repeat(20_000)),
        format!("RETURN toInteger(0 < 1{})", " <= 1".repeat(20_000)),
    ];
    on_small_stack(|| {
        for text in &texts {
            assert_eq!(single(&db, text), int(1), "{}", &text[..40]);
        }
    });
}

#[test]
fn nesting_just_inside_the_parser_limit_runs_on_a_small_stack() {
    let test = "nesting_just_inside_the_parser_limit_runs_on_a_small_stack";
    if !isolated(test, CHILD_LIMIT) {
        return;
    }
    let db = db();
    let texts: Vec<String> = [20, 40, 63, 64, 80, 100, 120, 126, 127, 128]
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
                format!("RETURN {}1{}", "{a: ".repeat(depth), "}".repeat(depth)),
                format!("RETURN null{}", "[0]".repeat(depth)),
                format!("RETURN null{}", "[0..1]".repeat(depth)),
                format!("RETURN null{}", ".a".repeat(depth)),
                format!("RETURN {}1", "- ".repeat(depth)),
                format!("RETURN true{}", " AND true".repeat(depth)),
                format!("RETURN 1{}", " IS NULL".repeat(depth)),
                format!(
                    "RETURN {}1{}",
                    "[x IN [".repeat(depth),
                    "] | x]".repeat(depth)
                ),
                format!(
                    "RETURN {}true{}",
                    "all(x IN [".repeat(depth),
                    "] WHERE x)".repeat(depth)
                ),
                format!(
                    "RETURN {}true{}",
                    "exists { MATCH (a) WHERE ".repeat(depth),
                    " }".repeat(depth)
                ),
                format!(
                    "MATCH (a) WHERE {}true{} RETURN a",
                    "exists { (a)--() WHERE ".repeat(depth),
                    " }".repeat(depth)
                ),
                format!(
                    "RETURN {}1{}",
                    "COUNT { MATCH (a) WHERE ".repeat(depth),
                    " = 0 }".repeat(depth)
                ),
                format!(
                    "RETURN {}1{}",
                    "size(COLLECT { RETURN ".repeat(depth),
                    " }) ".repeat(depth)
                ),
                format!(
                    "{}RETURN 1 AS x{}",
                    "CALL { ".repeat(depth),
                    " } RETURN x".repeat(depth)
                ),
                format!(
                    "{}INSERT (:D){}",
                    "CALL { ".repeat(depth),
                    " }".repeat(depth)
                ),
                format!(
                    "RETURN {}1{}",
                    "VALUE { RETURN ".repeat(depth),
                    " }".repeat(depth)
                ),
                format!(
                    "RETURN {}1{}",
                    "CAST(".repeat(depth),
                    " AS INTEGER)".repeat(depth)
                ),
                format!(
                    "RETURN [1] IS :: {}INTEGER{}",
                    "LIST<".repeat(depth),
                    ">".repeat(depth)
                ),
                format!("RETURN 1{}", " IS :: ANY".repeat(depth)),
                format!("RETURN true{}", " IS DISTINCT FROM true".repeat(depth)),
                format!("RETURN true{}", " IS NOT TRUE".repeat(depth)),
                format!(
                    "{}RETURN 1 AS x{}",
                    "CALL { RETURN 1 AS y NEXT ".repeat(depth),
                    " } RETURN x".repeat(depth)
                ),
                format!(
                    "UNWIND [1] AS i {}INSERT (:D {{i: i}}) RETURN i AS j{}",
                    "CALL (i) { ".repeat(depth),
                    " } RETURN 1 AS k".repeat(depth)
                ),
                format!(
                    "RETURN {}0{}",
                    "reduce(a = ".repeat(depth),
                    ", x IN [1] | a + x)".repeat(depth)
                ),
                format!(
                    "RETURN {}1{}",
                    "CASE 1 WHEN > 0, 2 THEN ".repeat(depth),
                    " END".repeat(depth)
                ),
                format!(
                    "MATCH (a) RETURN {}1{}",
                    "[(a)--() | ".repeat(depth),
                    "]".repeat(depth)
                ),
                format!(
                    "MATCH (n) WHERE n:{}A{} RETURN n",
                    "(".repeat(depth),
                    ")".repeat(depth)
                ),
                format!(
                    "RETURN 'a' =~ '{}a{}'",
                    "(".repeat(depth.min(64)),
                    ")".repeat(depth.min(64))
                ),
                format!(
                    "UNWIND [1] AS x WITH x, count(*) AS c ORDER BY {}x{} RETURN c",
                    "[".repeat(depth),
                    "]".repeat(depth)
                ),
                format!(
                    "INSERT (n:N {{p: {}1{}}}) SET n.q = {}true RETURN n.q",
                    "(".repeat(depth),
                    ")".repeat(depth),
                    "NOT ".repeat(depth)
                ),
            ]
        })
        .collect();
    on_stack(NESTING_STACK, || {
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
        assert_eq!(
            single(
                &db,
                "MATCH p = (:C {i: 20000})<-[:NEXT*15000..]-(b) RETURN sum(length(p))"
            ),
            int((15_000..=20_000).sum::<i64>())
        );
        assert_eq!(
            single(
                &db,
                "MATCH (a:C {i: 0}) RETURN exists { (a)-[*19999..]->(b) WHERE b.i > 0 }"
            ),
            boolean(true)
        );
        let text = format!(
            "MATCH (a:C {{i: 0}}){} RETURN count(*)",
            "-->()".repeat(20_000)
        );
        assert_eq!(single(&db, &text), int(1));
        let text = format!(
            "MATCH (a:C {{i: 0}}){}, (b:C {{i: 20000}}) RETURN count(*)",
            "-[*1]->()".repeat(20_000)
        );
        assert_eq!(single(&db, &text), int(1));
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
fn strings_and_lists_a_query_builds_stop_at_their_length_limits() {
    let test = "strings_and_lists_a_query_builds_stop_at_their_length_limits";
    if !isolated(test, CHILD_LIMIT) {
        return;
    }
    let db = db();
    let wide = Parameters::new().with("text", "x".repeat(20_000));
    for text in [
        "RETURN size(reduce(s = 'x', i IN range(1, 45) | s + s))",
        "RETURN size(reduce(s = 'x', i IN range(1, 45) | s || s))",
        "RETURN size(reduce(s = 1, i IN range(1, 45) | s + toString(s)))",
        "RETURN size(range(1, 10000000) + 0)",
        "RETURN size(0 + range(1, 10000000))",
        "RETURN size(range(1, 10000000) + [0])",
        "RETURN size(range(1, 10000000) || [0])",
        "RETURN size(replace($text, '', $text))",
        "RETURN size(replace($text, 'x', $text))",
    ] {
        assert_eq!(
            db.execute(text, &wide).unwrap_err().kind(),
            ErrorKind::NumberOutOfRange,
            "{text}"
        );
    }
    assert_eq!(
        db.execute(
            "RETURN size(replace($text, 'x', 'yy')), size(range(1, 9999999) + 0)",
            &wide
        )
        .unwrap()
        .rows(),
        [vec![int(40_000), int(10_000_000)]]
    );
}

#[test]
fn nested_comprehensions_and_repeated_appends_stop_at_the_default_limits() {
    let test = "nested_comprehensions_and_repeated_appends_stop_at_the_default_limits";
    if !isolated(test, CHILD_LIMIT) {
        return;
    }
    let db = db();
    for text in [
        "RETURN size([x IN range(1, 100000) | range(1, 1000)])",
        "UNWIND range(1, 100000) AS i RETURN size(collect(range(1, 1000)))",
        "RETURN size(reduce(l = [], i IN range(1, 100000) | l + [range(1, 1000)]))",
    ] {
        assert_eq!(error_kind(&db, text), ErrorKind::NumberOutOfRange, "{text}");
    }
    assert_eq!(
        single(
            &db,
            "RETURN size(reduce(l = [], i IN range(1, 1000000) | l + i))"
        ),
        int(1_000_000)
    );
    assert_eq!(
        single(
            &db,
            "RETURN size(reduce(s = '', i IN range(1, 1000000) | s + 'x'))"
        ),
        int(1_000_000)
    );
}

fn limited(items: usize, bytes: usize) -> Database {
    options()
        .max_value_items(items)
        .max_value_bytes(bytes)
        .in_memory()
        .unwrap()
}

#[test]
fn every_way_a_query_grows_a_value_stops_at_the_configured_limits() {
    let db = limited(100, 1000);
    run(
        &db,
        "INSERT (a:A), (a)-[:R]->(:B), (a)-[:R]->(:B) \
         WITH 1 AS x UNWIND range(1, 60) AS i INSERT (:N {i: i})",
    );
    let text = Parameters::new()
        .with("upper", "\u{149}".repeat(400))
        .with("ligature", "\u{fdfa}".repeat(100))
        .with("wide", "x".repeat(101))
        .with(
            "nested",
            Value::List(vec![Value::List((1..=60).map(Value::Integer).collect()); 2]),
        );
    for query in [
        "RETURN range(1, 101)",
        "RETURN [range(1, 60), range(1, 60)]",
        "RETURN {a: range(1, 60), b: range(1, 60)}",
        "RETURN range(1, 60) + range(1, 60)",
        "RETURN range(1, 60) || range(1, 60)",
        "RETURN [range(1, 99)] + 1",
        "RETURN 1 + [range(1, 99)]",
        "RETURN [x IN range(1, 10) | range(1, 10)]",
        "RETURN reduce(l = [], i IN range(1, 60) | l + [i, i])",
        "RETURN reduce(l = [], i IN range(1, 60) | l || [i, i])",
        "RETURN reduce(l = [], i IN range(1, 30) | l + [[i, i, i]])",
        "RETURN reduce(l = [0], i IN range(1, 10) | l + l)",
        "RETURN reduce(l = [0], i IN range(1, 10) | [l, l])",
        "RETURN reduce(m = {}, i IN range(1, 10) | {a: m, b: m})",
        "WITH range(1, 30) AS l RETURN [l, l, l, l]",
        "UNWIND range(1, 50) AS i UNWIND range(1, 3) AS j RETURN collect(i)",
        "UNWIND range(1, 20) AS i RETURN collect([i, i, i, i, i])",
        "MATCH (n:N) RETURN collect([n.i, n.i])",
        "RETURN COLLECT { UNWIND range(1, 60) AS i RETURN [i, i] }",
        "MATCH (a:A) RETURN [(a)-[:R]->(b) | range(1, 60)]",
        "RETURN CAST($nested AS LIST<LIST<STRING>>)",
        "RETURN split($wide, '')",
        "RETURN reduce(s = '', i IN range(1, 100) | s + '0123456789x')",
        "RETURN reduce(s = '', i IN range(1, 100) | s || '0123456789x')",
        "RETURN [x IN range(1, 51) | 'abcdefghijklmnopqrst']",
        "UNWIND range(1, 51) AS i RETURN collect('abcdefghijklmnopqrst')",
        "RETURN replace($wide, 'x', 'xxxxxxxxxx')",
        "RETURN toUpper($upper)",
        "RETURN normalize($ligature, NFKD)",
        "UNWIND range(1, 5) AS i RETURN i ORDER BY range(1, 101) LIMIT 2",
        "UNWIND range(1, 5) AS i RETURN range(1, 101) AS l ORDER BY i DESC LIMIT 1",
        "MATCH (n:N) RETURN n.i ORDER BY CASE n.i WHEN 60 THEN range(1, 101) END SKIP 1 LIMIT 1",
        "MATCH (n:N) WITH n ORDER BY n.i LIMIT 0 RETURN count(*) + size(range(1, 101))",
    ] {
        assert_eq!(
            db.execute(query, &text).unwrap_err().kind(),
            ErrorKind::NumberOutOfRange,
            "{query}"
        );
    }
    for (query, expected) in [
        ("RETURN size(range(1, 100))", 100),
        ("RETURN size([x IN range(1, 9) | range(1, 10)])", 9),
        (
            "RETURN size(reduce(l = [], i IN range(1, 50) | l + [i, i]))",
            100,
        ),
        (
            "UNWIND range(1, 50) AS i UNWIND range(1, 2) AS j RETURN size(collect(i))",
            100,
        ),
        (
            "MATCH (n:N) WHERE n.i <= 50 RETURN size(collect([n.i]))",
            50,
        ),
        (
            "RETURN size(reduce(s = '', i IN range(1, 100) | s + '0123456789'))",
            1000,
        ),
        ("RETURN size(toUpper($wide))", 101),
        ("RETURN size(reduce(l = [0], i IN range(1, 5) | [l, l]))", 2),
    ] {
        assert_eq!(
            db.execute(query, &text).unwrap().rows(),
            [vec![int(expected)]],
            "{query}"
        );
    }
    let short = limited(100, 10);
    assert_eq!(single(&short, "RETURN size(keys({abcdefghij: 1}))"), int(1));
    for query in ["RETURN {abcdefghijk: 1}", "RETURN [{abcdef: 1, ghijk: 2}]"] {
        assert_eq!(
            error_kind(&short, query),
            ErrorKind::NumberOutOfRange,
            "{query}"
        );
    }
}

#[test]
fn collect_stops_at_the_limits_on_one_thread_and_split_across_four() {
    let serial = OpenOptions::new().threads(1);
    let split = OpenOptions::new().threads(4).parallel_threshold(2);
    for options in [serial, split] {
        let db = options
            .max_value_items(50)
            .max_value_bytes(100)
            .in_memory()
            .unwrap();
        run(
            &db,
            "UNWIND range(0, 1) AS j UNWIND range(1, 30) AS k WITH j * 30 + k AS i \
             INSERT (:N {i: i, s: toString(i % 10)})",
        );
        for query in [
            "MATCH (n:N) RETURN collect(n.i)",
            "MATCH (n:N) RETURN collect(DISTINCT n.i)",
            "MATCH (n:N) WHERE n.i <= 30 RETURN collect([n.i])",
            "MATCH (n:N) RETURN collect(n.s + n.s)",
            "MATCH (n:N) WHERE n.i <= 25 WITH collect(n.i) AS l RETURN l + l + 1",
        ] {
            assert_eq!(
                error_kind(&db, query),
                ErrorKind::NumberOutOfRange,
                "{query}"
            );
        }
        for (query, expected) in [
            ("MATCH (n:N) RETURN size(collect(DISTINCT n.i % 50))", 50),
            ("MATCH (n:N) WHERE n.i <= 50 RETURN size(collect(n.i))", 50),
            ("MATCH (n:N) RETURN size(collect(DISTINCT n.s + n.s))", 10),
            (
                "MATCH (n:N) WHERE n.i <= 25 WITH collect(n.i) AS l RETURN size(l + l)",
                50,
            ),
            ("MATCH (n:N) RETURN size(max(n.s + n.s + n.s))", 3),
        ] {
            assert_eq!(single(&db, query), int(expected), "{query}");
        }
    }
}

#[test]
fn appending_to_a_list_another_value_holds_leaves_that_value_as_it_was() {
    let db = limited(100, 1000);
    assert_eq!(
        db.execute(
            "WITH [1, 2] AS l \
             RETURN reduce(a = l, i IN range(1, 3) | a + i) AS grown, l, l + l AS twice",
            &Parameters::new(),
        )
        .unwrap()
        .rows(),
        [vec![
            Value::List((1..=2).chain(1..=3).map(Value::Integer).collect()),
            Value::List(vec![int(1), int(2)]),
            Value::List(vec![int(1), int(2), int(1), int(2)]),
        ]]
    );
    let shared = Parameters::new().with("l", Value::List((1..=40).map(Value::Integer).collect()));
    assert_eq!(
        db.execute(
            "RETURN size(reduce(a = $l, i IN range(1, 10) | a + i)) AS grown, size($l + $l) AS twice",
            &shared,
        )
        .unwrap()
        .rows(),
        [vec![int(50), int(80)]]
    );
    assert_eq!(
        db.execute("RETURN $l + $l + $l", &shared)
            .unwrap_err()
            .kind(),
        ErrorKind::NumberOutOfRange
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
    assert_eq!(
        run(
            &db,
            "UNWIND [1, 2] AS x RETURN x ORDER BY x SKIP 9223372036854775807 LIMIT 9223372036854775807"
        ),
        Vec::<Vec<Value>>::new()
    );
    assert_eq!(
        run(
            &db,
            "UNWIND [1, 2] AS x RETURN x ORDER BY x DESC SKIP 1 LIMIT 9223372036854775807"
        ),
        vec![vec![Value::Integer(1)]]
    );
}

#[test]
fn a_property_of_eight_megabytes_and_a_list_of_half_a_million_survive_reopening() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph.rhizodb");
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
#[ignore = "left out of mise run check, run by mise run random"]
fn a_write_transaction_of_many_statements_and_nodes_commits_whole_or_rolls_back_whole() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph.rhizodb");
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

/// Where the wide node of a run comes from.
#[derive(Clone, Copy, Debug)]
enum Wide {
    Committed,
    TouchedInTheTransaction,
    CreatedInTheTransaction,
}

/// The fastest of three runs of `work` in a write transaction on a node with `count`
/// properties.
fn fastest_on_a_node_with_properties(count: i64, work: &str, wide: Wide) -> Duration {
    let insert = "INSERT (n:Wide) SET n += $properties";
    (0..3)
        .map(|_| {
            let db = db();
            let properties = Parameters::from([(
                "properties".to_owned(),
                Value::Map(
                    (0..count)
                        .map(|i| (format!("p{i}"), Value::Integer(i)))
                        .collect(),
                ),
            )]);
            if !matches!(wide, Wide::CreatedInTheTransaction) {
                db.execute(insert, &properties).unwrap();
            }
            let mut tx = db.begin_write().unwrap();
            match wide {
                Wide::Committed => {}
                Wide::TouchedInTheTransaction => {
                    tx.execute("MATCH (n:Wide) SET n.extra = 1", &Parameters::new())
                        .unwrap();
                }
                Wide::CreatedInTheTransaction => {
                    tx.execute(insert, &properties).unwrap();
                }
            }
            let started = std::time::Instant::now();
            tx.execute(work, &Parameters::new()).unwrap();
            let elapsed = started.elapsed();
            tx.commit().unwrap();
            elapsed
        })
        .min()
        .unwrap()
}

#[test]
#[ignore = "left out of mise run check, run by mise run random"]
fn reading_or_deleting_every_property_of_a_node_takes_time_linear_in_their_number() {
    for work in [
        "MATCH (n:Wide) RETURN size(keys(properties(n)))",
        "MATCH (n:Wide) SET n = {}",
        "MATCH (n:Wide) DELETE n",
    ] {
        for wide in [
            Wide::Committed,
            Wide::TouchedInTheTransaction,
            Wide::CreatedInTheTransaction,
        ] {
            let small = fastest_on_a_node_with_properties(1_000, work, wide);
            let large = fastest_on_a_node_with_properties(16_000, work, wide);
            assert!(
                large < small * 100,
                "{work} on a node {wide:?}: {small:?} for 1000 properties but {large:?} for 16000"
            );
        }
    }
}

/// The fastest of three runs of `work` on a relationship with `count` committed properties.
fn fastest_on_a_relationship_with_properties(count: i64, work: &str) -> Duration {
    (0..3)
        .map(|_| {
            let db = db();
            let properties = Parameters::from([(
                "properties".to_owned(),
                Value::Map(
                    (0..count)
                        .map(|i| (format!("p{i}"), Value::Integer(i)))
                        .collect(),
                ),
            )]);
            db.execute("INSERT ()-[r:WIDE]->() SET r += $properties", &properties)
                .unwrap();
            let started = std::time::Instant::now();
            db.execute(work, &properties).unwrap();
            started.elapsed()
        })
        .min()
        .unwrap()
}

#[test]
#[ignore = "left out of mise run check, run by mise run random"]
fn replacing_every_property_of_a_relationship_takes_time_linear_in_their_number() {
    for work in [
        "MATCH ()-[r:WIDE]->() SET r = {}",
        "MATCH ()-[r:WIDE]->() SET r = {a: 1}",
        "MATCH ()-[r:WIDE]->() SET r += $properties",
        "MATCH ()-[r:WIDE]->() SET r = $properties",
    ] {
        let small = fastest_on_a_relationship_with_properties(2_000, work);
        let large = fastest_on_a_relationship_with_properties(32_000, work);
        assert!(
            large < small * 100,
            "{work}: {small:?} for 2000 properties but {large:?} for 32000"
        );
    }
}

fn directory_bytes(directory: &std::path::Path) -> u64 {
    std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().metadata().unwrap().len())
        .sum()
}

#[test]
fn setting_or_removing_many_labels_at_once_stores_no_label_set_in_between() {
    let directory = tempfile::tempdir().unwrap();
    let db = Database::open(directory.path().join("labels.rhizodb")).unwrap();
    let labels: String = (0..3_000).map(|i| format!(":L{i}")).collect();
    run(&db, &format!("INSERT (n:Start{labels})"));
    run(&db, "INSERT (:Other)");
    let before = directory_bytes(directory.path());
    run(&db, &format!("MATCH (n:Start) REMOVE n{labels}"));
    run(&db, &format!("MATCH (n:Other) SET n{labels}"));
    let grown = directory_bytes(directory.path()).saturating_sub(before);
    assert!(
        grown < 4 << 20,
        "setting and removing 3000 labels grew the files by {grown} bytes"
    );
    assert_eq!(
        run(
            &db,
            "MATCH (n) RETURN size(labels(n)) AS count ORDER BY count"
        ),
        [[int(1)], [int(3_001)]]
    );
}

#[test]
fn a_label_set_a_transaction_passes_through_is_never_stored() {
    let directory = tempfile::tempdir().unwrap();
    let db = Database::open(directory.path().join("labels.rhizodb")).unwrap();
    let labels: Vec<String> = (0..2_000).map(|i| format!("L{i}")).collect();
    run(&db, "INSERT (:Start), (:Other)");
    let before = directory_bytes(directory.path());
    let sets: String = labels
        .iter()
        .map(|label| format!(" SET n:{label}"))
        .collect();
    run(&db, &format!("MATCH (n:Start){sets}"));
    let mut tx = db.begin_write().unwrap();
    for label in &labels {
        tx.execute(
            &format!("MATCH (n:Other) SET n:{label}"),
            &Parameters::new(),
        )
        .unwrap();
    }
    tx.commit().unwrap();
    let removes: String = labels
        .iter()
        .map(|label| format!(" REMOVE n:{label}"))
        .collect();
    run(&db, &format!("MATCH (n:Other){removes}"));
    let grown = directory_bytes(directory.path()).saturating_sub(before);
    assert!(
        grown < 4 << 20,
        "adding and removing 2000 labels one clause at a time grew the files by {grown} bytes"
    );
    assert_eq!(
        run(
            &db,
            "MATCH (n) RETURN size(labels(n)) AS count ORDER BY count"
        ),
        [[int(1)], [int(2_001)]]
    );
    db.check().unwrap();
}

/// How long the fastest of three runs of `query` takes, each with a new query text so none
/// reuses the plan of another.
fn fastest(db: &Database, query: &str) -> Duration {
    (0..3)
        .map(|run_index| {
            let query = format!("{query} AS run{run_index}");
            let started = std::time::Instant::now();
            run(db, &query);
            started.elapsed()
        })
        .min()
        .unwrap()
}

#[test]
#[ignore = "left out of mise run check, run by mise run random"]
fn appending_to_a_list_takes_time_that_does_not_grow_with_what_its_items_hold() {
    let db = db();
    let appends = |inner: usize| {
        format!(
            "WITH range(1, {inner}) AS row WITH [x IN range(1, 200) | row] AS big \
             UNWIND range(1, 1000) AS i RETURN count(big + i)"
        )
    };
    let small = fastest(&db, &appends(50));
    let large = fastest(&db, &appends(2_000));
    assert!(
        large < small * 4,
        "{small:?} to append to lists of 200 lists of 50 but {large:?} for lists of 2000"
    );
}

#[test]
#[ignore = "left out of mise run check, run by mise run random"]
fn a_chain_of_let_or_with_clauses_takes_time_linear_in_its_length() {
    let db = db();
    let lets = |count: usize| {
        let chain: String = (1..count)
            .map(|i| format!(" LET x{i} = x{} + 1", i - 1))
            .collect();
        format!("UNWIND [0] AS x0{chain} RETURN x{}", count - 1)
    };
    let withs = |count: usize| {
        let chain = " WITH x WHERE x = 0".repeat(count);
        format!("UNWIND [0] AS x{chain} RETURN x")
    };
    for (name, query) in [("LET", &lets as &dyn Fn(usize) -> String), ("WITH", &withs)] {
        let small = fastest(&db, &query(1_000));
        let large = fastest(&db, &query(16_000));
        assert!(
            large < small * 64,
            "{small:?} for 1000 {name} clauses but {large:?} for 16000"
        );
    }
}

/// How long one write transaction takes to run `statements` statements that each match one
/// node by its label and insert a node of another label.
fn label_matches_in_one_transaction(statements: i64) -> Duration {
    let db = db();
    run(&db, "INSERT (:Hub)");
    let started = std::time::Instant::now();
    let mut tx = db.begin_write().unwrap();
    for i in 0..statements {
        tx.execute(
            "MATCH (h:Hub) INSERT (:Leaf {i: $i})",
            &Parameters::from([("i".to_owned(), Value::Integer(i))]),
        )
        .unwrap();
    }
    tx.commit().unwrap();
    started.elapsed()
}

#[test]
#[ignore = "left out of mise run check, run by mise run random"]
fn a_label_scan_in_a_write_transaction_skips_the_nodes_it_created_without_the_label() {
    let small = (0..3)
        .map(|_| label_matches_in_one_transaction(2_000))
        .min()
        .unwrap();
    let large = (0..3)
        .map(|_| label_matches_in_one_transaction(32_000))
        .min()
        .unwrap();
    assert!(
        large < small * 100,
        "{small:?} for 2000 statements but {large:?} for 32000"
    );
}
