use super::parse;
use crate::gql::ast::Call;
use crate::gql::ast::Clause;
use crate::gql::ast::Delete;
use crate::gql::ast::Expr;
use crate::gql::ast::Imports;
use crate::gql::ast::LabelExpr;
use crate::gql::ast::Literal;
use crate::gql::ast::Name;
use crate::gql::ast::ProjectionItems;
use crate::gql::ast::Query;
use crate::gql::ast::RemoveItem;
use crate::gql::ast::SetItem;
use crate::gql::ast::SortDirection;
use crate::gql::ast::Span;
use crate::gql::ast::UnionKind;
use crate::gql::ast::Yield;
use crate::gql::ast::YieldItem;
use crate::gql::error::SyntaxErrorKind;
use crate::gql::testing::error;
use crate::gql::testing::sexp;
use crate::gql::testing::single_query_clauses;

fn name(text: &str) -> Name {
    Name::new(text)
}

fn var(text: &str) -> Expr {
    Expr::Variable(name(text))
}

fn int(value: i64) -> Expr {
    Expr::Literal(Literal::Integer(value))
}

fn only_clause(source: &str) -> Clause {
    let mut clauses = single_query_clauses(source);
    assert_eq!(clauses.len(), 1, "{source}");
    clauses.remove(0)
}

fn only_return(source: &str) -> crate::gql::ast::Projection {
    match only_clause(source) {
        Clause::Return(projection) => projection,
        other => panic!("expected RETURN, got {other:?}"),
    }
}

fn items(projection: &crate::gql::ast::Projection) -> &[crate::gql::ast::ProjectionItem] {
    match &projection.items {
        ProjectionItems::Items(items) | ProjectionItems::Star(items) => items,
    }
}

#[test]
fn match_with_where_and_several_patterns() {
    let Clause::Match(found) = only_clause("MATCH (a), (b)-->(c) WHERE a.x = 1") else {
        panic!("expected MATCH");
    };
    assert!(!found.optional);
    assert_eq!(found.patterns.len(), 2);
    assert_eq!(
        found.filter.as_ref().map(sexp).as_deref(),
        Some("(cmp (. a x) = 1)")
    );
}

#[test]
fn optional_match_sets_the_flag() {
    let Clause::Match(found) = only_clause("OPTIONAL MATCH (a)") else {
        panic!("expected MATCH");
    };
    assert!(found.optional);
    assert!(found.filter.is_none());
}

#[test]
fn insert_and_create_produce_the_same_clause() {
    let insert = only_clause("INSERT (:A {k: 1})-[:T]->(:B)");
    let create = only_clause("CREATE (:A {k: 1})-[:T]->(:B)");
    assert_eq!(insert, create);
    assert!(matches!(insert, Clause::Insert(parts) if parts.len() == 1));
}

#[test]
fn return_items_aliases_and_column_spans() {
    let projection = only_return("RETURN a, b.c AS d, 1 + 2");
    let found = items(&projection);
    assert_eq!(found.len(), 3);
    assert_eq!(found[0].alias, None);
    assert_eq!(found[1].alias, Some(name("d")));
    assert_eq!(found[1].span, Span { start: 10, end: 13 });
    assert_eq!(found[2].span, Span { start: 20, end: 25 });
    assert!(!projection.distinct);
}

#[test]
fn return_distinct_and_star() {
    assert!(only_return("RETURN DISTINCT a").distinct);
    assert!(
        matches!(only_return("RETURN *").items, ProjectionItems::Star(extra) if extra.is_empty())
    );
    assert!(
        matches!(only_return("RETURN *, a AS b").items, ProjectionItems::Star(extra) if extra.len() == 1)
    );
}

#[test]
fn order_by_with_every_direction_spelling() {
    let projection = only_return("RETURN a ORDER BY a, b ASC, c DESC, d ASCENDING, e DESCENDING");
    let directions: Vec<SortDirection> = projection.order_by.iter().map(|s| s.direction).collect();
    assert_eq!(
        directions,
        vec![
            SortDirection::Ascending,
            SortDirection::Ascending,
            SortDirection::Descending,
            SortDirection::Ascending,
            SortDirection::Descending,
        ]
    );
}

#[test]
fn skip_offset_and_limit_take_expressions() {
    let skipped = only_return("RETURN a SKIP 2 LIMIT $n");
    assert_eq!(skipped.skip, Some(int(2)));
    assert_eq!(skipped.limit, Some(Expr::Parameter(name("n"))));
    let offset = only_return("RETURN a ORDER BY a OFFSET 1 + 1 LIMIT 3");
    assert_eq!(offset.skip.as_ref().map(sexp).as_deref(), Some("(+ 1 1)"));
    assert_eq!(offset.limit, Some(int(3)));
}

#[test]
fn with_takes_a_projection_and_a_where() {
    let Clause::With(found) =
        only_clause("WITH DISTINCT a, count(*) AS c ORDER BY c LIMIT 5 WHERE c > 1")
    else {
        panic!("expected WITH");
    };
    assert!(found.projection.distinct);
    assert_eq!(found.projection.limit, Some(int(5)));
    assert_eq!(
        found.filter.as_ref().map(sexp).as_deref(),
        Some("(cmp c > 1)")
    );
}

#[test]
fn unwind_binds_a_variable() {
    assert_eq!(
        only_clause("UNWIND [1, 2] AS x"),
        Clause::Unwind(crate::gql::ast::Unwind {
            expr: Expr::List(vec![int(1), int(2)]),
            variable: name("x"),
        })
    );
}

#[test]
fn filter_takes_a_condition_with_or_without_where() {
    for source in ["FILTER k > 1", "FILTER WHERE k > 1", "filter where k > 1"] {
        let Clause::Filter(condition) = only_clause(source) else {
            panic!("expected FILTER in {source}");
        };
        assert_eq!(sexp(&condition), "(cmp k > 1)", "{source}");
    }
}

#[test]
fn filter_accepts_a_pattern_predicate() {
    let clauses = single_query_clauses("MATCH (a) FILTER (a)-->() RETURN a");
    assert!(
        matches!(&clauses[1], Clause::Filter(Expr::Pattern(_))),
        "{clauses:?}"
    );
}

#[test]
fn filter_errors_report_offsets() {
    for (source, expected) in [
        ("FILTER", (6, SyntaxErrorKind::UnexpectedEnd)),
        ("FILTER WHERE", (12, SyntaxErrorKind::UnexpectedEnd)),
        ("FILTER RETURN 1", (7, SyntaxErrorKind::UnexpectedToken)),
        (
            "FILTER WHERE WHERE k",
            (13, SyntaxErrorKind::UnexpectedToken),
        ),
        (
            "FILTER k RETURN k k",
            (18, SyntaxErrorKind::UnexpectedToken),
        ),
    ] {
        assert_eq!(error(source), expected, "{source:?}");
    }
}

#[test]
fn let_binds_variables_in_source_order() {
    let Clause::Let(bindings) = only_clause("let a = 1, b = k * 2") else {
        panic!("expected LET");
    };
    let found: Vec<(&str, String)> = bindings
        .iter()
        .map(|(variable, value)| (variable.0.as_str(), sexp(value)))
        .collect();
    assert_eq!(found, [("a", "1".into()), ("b", "(* k 2)".into())]);
}

#[test]
fn let_errors_report_offsets() {
    for (source, expected) in [
        ("LET", (3, SyntaxErrorKind::UnexpectedEnd)),
        ("LET a", (5, SyntaxErrorKind::UnexpectedEnd)),
        ("LET a 1", (6, SyntaxErrorKind::UnexpectedToken)),
        ("LET a = ", (8, SyntaxErrorKind::UnexpectedEnd)),
        ("LET = 1", (4, SyntaxErrorKind::UnexpectedToken)),
        ("LET a = 1,", (10, SyntaxErrorKind::UnexpectedEnd)),
        ("LET a.b = 1", (5, SyntaxErrorKind::UnexpectedToken)),
        ("LET a = 1 b = 2", (10, SyntaxErrorKind::UnexpectedToken)),
    ] {
        assert_eq!(error(source), expected, "{source:?}");
    }
}

#[test]
fn set_has_property_map_and_label_forms() {
    let Clause::Set(found) = only_clause("SET n.a = 1, n = {b: 2}, n += $m, n:A:B") else {
        panic!("expected SET");
    };
    assert_eq!(
        found,
        vec![
            SetItem::Property {
                base: var("n"),
                key: name("a"),
                value: int(1)
            },
            SetItem::ReplaceProperties {
                variable: name("n"),
                value: Expr::Map(vec![(name("b"), int(2))]),
            },
            SetItem::MergeProperties {
                variable: name("n"),
                value: Expr::Parameter(name("m")),
            },
            SetItem::Labels {
                variable: name("n"),
                labels: LabelExpr::And(vec![
                    LabelExpr::Name(name("A")),
                    LabelExpr::Name(name("B"))
                ])
            },
        ]
    );
}

#[test]
fn set_property_on_a_nested_base() {
    let Clause::Set(found) = only_clause("SET (n).a.b = 1") else {
        panic!("expected SET");
    };
    assert!(matches!(
        &found[0],
        SetItem::Property { base, key, .. } if sexp(base) == "(. n a)" && key.0 == "b"
    ));
}

#[test]
fn set_rejects_a_target_that_is_not_a_property() {
    assert_eq!(error("SET n[0] = 1"), (4, SyntaxErrorKind::UnexpectedToken));
}

#[test]
fn remove_has_property_and_label_forms() {
    assert_eq!(
        only_clause("REMOVE n.a, n:A:B"),
        Clause::Remove(vec![
            RemoveItem::Property {
                base: var("n"),
                key: name("a")
            },
            RemoveItem::Labels {
                variable: name("n"),
                labels: LabelExpr::And(vec![
                    LabelExpr::Name(name("A")),
                    LabelExpr::Name(name("B"))
                ])
            },
        ])
    );
}

#[test]
fn delete_and_detach_delete() {
    assert_eq!(
        only_clause("DELETE a, b.c"),
        Clause::Delete(Delete {
            detach: false,
            targets: vec![
                var("a"),
                Expr::Property {
                    base: Box::new(var("b")),
                    key: name("c")
                }
            ],
        })
    );
    assert!(matches!(only_clause("DETACH DELETE n"), Clause::Delete(d) if d.detach));
}

#[test]
fn merge_collects_on_create_and_on_match_actions() {
    let Clause::Merge(found) = only_clause(
        "MERGE (n:A {k: 1}) ON CREATE SET n.c = 1 ON MATCH SET n.m = 2, n.m2 = 3 ON CREATE SET n.c2 = 4",
    ) else {
        panic!("expected MERGE");
    };
    assert_eq!(found.on_create.len(), 2);
    assert_eq!(found.on_match.len(), 2);
    assert_eq!(
        found.pattern.element.start.labels,
        Some(LabelExpr::Name(name("A")))
    );
}

#[test]
fn clauses_keep_their_source_order() {
    let clauses = single_query_clauses(
        "MATCH (a) WITH a UNWIND [1] AS x FILTER x > 0 LET y = x CREATE (b) SET b.k = x REMOVE b.k DELETE b \
         RETURN a",
    );
    let kinds: Vec<&str> = clauses
        .iter()
        .map(|clause| match clause {
            Clause::Match(_) => "match",
            Clause::Merge(_) => "merge",
            Clause::Insert(_) => "insert",
            Clause::With(_) => "with",
            Clause::Return(_) => "return",
            Clause::Unwind(_) => "unwind",
            Clause::Set(_) => "set",
            Clause::Remove(_) => "remove",
            Clause::Delete(_) => "delete",
            Clause::Call(_) => "call",
            Clause::Filter(_) => "filter",
            Clause::Let(_) => "let",
            Clause::CallSubquery(_) => "call subquery",
        })
        .collect();
    assert_eq!(
        kinds,
        vec![
            "match", "with", "unwind", "filter", "let", "insert", "set", "remove", "delete",
            "return"
        ]
    );
}

#[test]
fn union_collects_parts_under_one_kind() {
    let Ok(Query::Union(found)) = parse("RETURN 1 AS a UNION RETURN 2 AS a UNION RETURN 3 AS a")
    else {
        panic!("expected a union");
    };
    assert_eq!(found.kind, UnionKind::Distinct);
    assert_eq!(found.rest.len(), 2);
    let Ok(Query::Union(found)) = parse("RETURN 1 AS a union all RETURN 2 AS a") else {
        panic!("expected a union");
    };
    assert_eq!(found.kind, UnionKind::All);
}

#[test]
fn mixing_union_and_union_all_is_rejected_at_the_second_union() {
    assert_eq!(
        error("RETURN 1 AS a UNION RETURN 2 AS a UNION ALL RETURN 3 AS a"),
        (34, SyntaxErrorKind::MixedUnion)
    );
}

#[test]
fn a_trailing_semicolon_is_allowed() {
    assert!(parse("MATCH (n) RETURN n;").is_ok());
    assert_eq!(
        error("RETURN 1; RETURN 2"),
        (10, SyntaxErrorKind::UnexpectedToken)
    );
}

#[test]
fn clause_keywords_ignore_case_but_variables_do_not() {
    let lower =
        single_query_clauses("match (n) where n.x = 1 return n order by n.x desc skip 1 limit 2");
    let upper =
        single_query_clauses("MATCH (n) WHERE n.x = 1 RETURN n ORDER BY n.x DESC SKIP 1 LIMIT 2");
    assert_eq!(lower, upper);
    assert_ne!(
        single_query_clauses("RETURN n"),
        single_query_clauses("RETURN N")
    );
}

#[test]
fn whitespace_and_comments_between_tokens_do_not_change_the_tree() {
    assert_eq!(
        single_query_clauses("MATCH(n)//c\n/*x*/WHERE\tn.x\n=1"),
        single_query_clauses("MATCH (n) WHERE n.x = 1")
    );
}

#[test]
fn query_level_errors_report_offsets() {
    for (source, expected) in [
        ("", (0, SyntaxErrorKind::UnexpectedEnd)),
        ("   ", (3, SyntaxErrorKind::UnexpectedEnd)),
        ("RETURN", (6, SyntaxErrorKind::UnexpectedEnd)),
        ("RETURN 1 2", (9, SyntaxErrorKind::UnexpectedToken)),
        (
            "MATCH (n) RETURN n n",
            (19, SyntaxErrorKind::UnexpectedToken),
        ),
        ("(n)", (0, SyntaxErrorKind::UnexpectedToken)),
        ("RETURN 1 UNION", (14, SyntaxErrorKind::UnexpectedEnd)),
        ("UNWIND [1] x", (11, SyntaxErrorKind::UnexpectedToken)),
        ("RETURN a ORDER a", (15, SyntaxErrorKind::UnexpectedToken)),
        ("DETACH n", (7, SyntaxErrorKind::UnexpectedToken)),
        ("RETURN ORDER", (7, SyntaxErrorKind::UnexpectedToken)),
        (
            "MERGE (n) ON DELETE SET n.a = 1",
            (13, SyntaxErrorKind::UnexpectedToken),
        ),
    ] {
        assert_eq!(error(source), expected, "{source:?}");
    }
}

#[test]
fn clauses_outside_the_supported_set_are_reported_as_unsupported() {
    for source in ["FOREACH (x IN [1] | CREATE (:A))", "LOAD CSV FROM 'f' AS r"] {
        assert_eq!(
            error(source),
            (0, SyntaxErrorKind::UnsupportedFeature),
            "{source}"
        );
    }
}

#[test]
fn call_subqueries_take_every_variable_scope_clause() {
    for (source, optional, imports) in [
        ("MATCH (n) CALL { RETURN 1 AS x } RETURN x", false, None),
        (
            "MATCH (n) CALL (n) { RETURN 1 AS x } RETURN x",
            false,
            Some(Imports::Variables(vec![name("n")])),
        ),
        (
            "MATCH (n) CALL () { RETURN 1 AS x } RETURN x",
            false,
            Some(Imports::Variables(Vec::new())),
        ),
        (
            "MATCH (n) OPTIONAL CALL (*) { RETURN 1 AS x } RETURN x",
            true,
            Some(Imports::All),
        ),
    ] {
        let Query::Single(query) = parse(source).unwrap() else {
            panic!("{source}");
        };
        let Clause::CallSubquery(call) = &query.clauses[1] else {
            panic!("{source}: {:?}", query.clauses[1]);
        };
        assert_eq!(
            (call.optional, &call.imports),
            (optional, &imports),
            "{source}"
        );
    }
}

#[test]
fn call_takes_a_qualified_name_optional_arguments_and_yield_items() {
    assert_eq!(
        only_clause("CALL db.labels"),
        Clause::Call(Call {
            name: vec![name("db"), name("labels")],
            args: None,
            yields: None,
        })
    );
    assert_eq!(
        only_clause("CALL p() YIELD *"),
        Clause::Call(Call {
            name: vec![name("p")],
            args: Some(Vec::new()),
            yields: Some(Yield::Star),
        })
    );
    let clauses = single_query_clauses("CALL p(1, x) YIELD a AS b, c WHERE c RETURN b");
    assert_eq!(
        clauses.first(),
        Some(&Clause::Call(Call {
            name: vec![name("p")],
            args: Some(vec![int(1), var("x")]),
            yields: Some(Yield::Items {
                items: vec![
                    YieldItem {
                        output: name("a"),
                        alias: Some(name("b")),
                    },
                    YieldItem {
                        output: name("c"),
                        alias: None,
                    },
                ],
                filter: Some(var("c")),
            }),
        }))
    );
}

#[test]
fn yield_star_only_ends_a_query_that_is_one_call() {
    assert_eq!(
        error("CALL p() YIELD * RETURN a"),
        (17, SyntaxErrorKind::UnexpectedToken)
    );
    assert_eq!(
        error("MATCH (n) CALL p() YIELD *"),
        (25, SyntaxErrorKind::UnexpectedToken)
    );
}

#[test]
fn lexical_errors_keep_their_own_kind_and_offset() {
    assert_eq!(
        error("RETURN 'abc"),
        (7, SyntaxErrorKind::UnterminatedString)
    );
    assert_eq!(
        error("RETURN 1 ? 2"),
        (9, SyntaxErrorKind::UnexpectedCharacter)
    );
    assert_eq!(
        error("RETURN 12ab"),
        (7, SyntaxErrorKind::InvalidNumberLiteral)
    );
}

#[test]
fn deep_nesting_is_an_error_not_a_stack_overflow() {
    let depth = 100_000;
    let inputs = [
        format!("RETURN {}1{}", "(".repeat(depth), ")".repeat(depth)),
        format!("RETURN {}1{}", "[".repeat(depth), "]".repeat(depth)),
        format!("RETURN {}1", "-".repeat(depth)),
        format!("RETURN {}true", "NOT ".repeat(depth)),
        format!("RETURN {}1", "f(".repeat(depth)),
        format!("RETURN {}1", "{a: ".repeat(depth)),
        format!("RETURN 1{}", " + 1".repeat(depth)),
        format!("RETURN a{}", ".b".repeat(depth)),
        format!("RETURN a{}", "[0]".repeat(depth)),
        format!(
            "RETURN {}1{}",
            "CASE WHEN ".repeat(depth),
            " THEN 1 END".repeat(depth)
        ),
        format!("RETURN {}", "exists { MATCH (a) WHERE ".repeat(depth)),
        format!("RETURN 1 IS TYPED INT{}", " LIST".repeat(depth)),
        format!("RETURN 1 IS :: ANY<INT ARRAY{}>", " LIST".repeat(depth)),
    ];
    for input in &inputs {
        let (_, kind) = error(input);
        assert_eq!(kind, SyntaxErrorKind::NestingTooDeep, "{}", &input[..30]);
    }
}

#[test]
fn nesting_within_the_limit_still_parses() {
    let depth = 40;
    let nested = format!("RETURN {}1{}", "(".repeat(depth), ")".repeat(depth));
    assert!(parse(&nested).is_ok());
    let list = format!("RETURN {}1{}", "[".repeat(depth), "]".repeat(depth));
    assert!(parse(&list).is_ok());
}

#[test]
fn long_flat_chains_of_logic_and_comparison_parse() {
    for (first, operator) in [
        ("true", " OR true"),
        ("true", " AND true"),
        ("true", " XOR true"),
        ("1", " < 2"),
        ("1", " = 1"),
    ] {
        let input = format!("RETURN {first}{}", operator.repeat(100_000));
        assert!(parse(&input).is_ok(), "{operator}");
    }
    let nested = format!("RETURN {}true{}", "(true OR ".repeat(30), ")".repeat(30));
    assert!(parse(&nested).is_ok());
    let too_deep = format!(
        "RETURN {}true{}",
        "(true OR true OR true OR ".repeat(100),
        ")".repeat(100)
    );
    assert_eq!(error(&too_deep).1, SyntaxErrorKind::NestingTooDeep);
}

#[test]
fn nested_trim_calls_parse_in_time_linear_in_their_depth() {
    let depth = 50;
    let input = format!("RETURN {}'x'{}", "trim(".repeat(depth), ")".repeat(depth));
    let started = std::time::Instant::now();
    assert!(parse(&input).is_ok());
    let sided = format!(
        "RETURN {}'x'{}",
        "trim(LEADING 'a' FROM ".repeat(depth),
        ")".repeat(depth)
    );
    assert!(parse(&sided).is_ok());
    assert!(parse("RETURN trim(a IS DISTINCT FROM b)").is_ok());
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
}

#[test]
fn arbitrary_garbage_never_panics() {
    for source in [
        "\u{0}",
        "\u{FFFF}",
        "((((",
        "))))",
        "'",
        "\"",
        "`",
        "/*",
        "$",
        "RETURN \u{1F600}",
        "MATCH (a)-[",
        "MATCH (a)<",
        "RETURN 0x",
        "RETURN CASE",
        "RETURN [x IN",
        "RETURN {a:",
        "WITH *",
        "SET",
        "REMOVE",
        "DELETE",
        "UNWIND",
        "MERGE",
        "RETURN a[",
        "RETURN f(",
        "é",
    ] {
        let _ = parse(source);
    }
}
