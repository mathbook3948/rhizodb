mod support;

use std::sync::LazyLock;

use proptest::prelude::*;
use rhizodb::Database;
use rhizodb::ErrorKind;
use rhizodb::Parameters;
use rhizodb::Procedure;
use support::*;

const PLAIN: &str = "zz";

const KEYWORDS: &[&str] = &[
    "ALL",
    "AND",
    "ANY",
    "AS",
    "ASC",
    "ASCENDING",
    "BY",
    "CALL",
    "CASE",
    "CONTAINS",
    "COUNT",
    "CREATE",
    "DELETE",
    "DESC",
    "DESCENDING",
    "DETACH",
    "DISTINCT",
    "ELSE",
    "END",
    "ENDS",
    "EXISTS",
    "FALSE",
    "FILTER",
    "FOREACH",
    "IN",
    "INSERT",
    "IS",
    "LABELED",
    "LET",
    "LIMIT",
    "LOAD",
    "MATCH",
    "MERGE",
    "NONE",
    "NOT",
    "NULL",
    "OFFSET",
    "ON",
    "OPTIONAL",
    "OR",
    "ORDER",
    "REMOVE",
    "RETURN",
    "SET",
    "SINGLE",
    "SKIP",
    "STARTS",
    "THEN",
    "TRUE",
    "UNION",
    "UNWIND",
    "WHEN",
    "WHERE",
    "WITH",
    "XOR",
    "YIELD",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Slot {
    Variable,
    NodeLabel,
    RelationshipType,
    PropertyKey,
    MapKey,
    Alias,
    YieldName,
    LabelExpression,
    ProcedureName,
}

use Slot::Alias;
use Slot::LabelExpression;
use Slot::MapKey;
use Slot::NodeLabel;
use Slot::ProcedureName;
use Slot::PropertyKey;
use Slot::RelationshipType;
use Slot::Variable;
use Slot::YieldName;

const TEMPLATES: &[(Slot, &str, &str)] = &[
    (Variable, "match", "MATCH (§) RETURN §.n ORDER BY §.n"),
    (Variable, "match", "MATCH (§:Other) RETURN §.n"),
    (Variable, "match", "MATCH ()-[§:Other]->() RETURN §.n"),
    (Variable, "match", "MATCH (§ IS Other) RETURN §.n"),
    (Variable, "match", "MATCH ()-[§ IS Other]->() RETURN §.n"),
    (Variable, "match", "MATCH §=(:Other)-->() RETURN length(§)"),
    (
        Variable,
        "where",
        "MATCH (§) WHERE §.n >= 2 RETURN §.n ORDER BY §.n",
    ),
    (
        Variable,
        "where",
        "MATCH (§) WHERE § IS NOT NULL AND §:Other RETURN §.n",
    ),
    (
        Variable,
        "return",
        "MATCH (§) RETURN size(labels(§)), §.n ORDER BY §.n",
    ),
    (
        Variable,
        "case",
        "MATCH (§) RETURN CASE WHEN §.n > 1 THEN 1 ELSE 0 END ORDER BY §.n",
    ),
    (
        Variable,
        "case",
        "MATCH (§) RETURN CASE §.n WHEN 1 THEN 'one' ELSE §.n END ORDER BY §.n",
    ),
    (
        Variable,
        "map",
        "MATCH (§) RETURN {a: §.n, b: [§.n]} ORDER BY §.n",
    ),
    (
        Variable,
        "comprehension",
        "RETURN [§ IN [1, 2, 3] WHERE § > 1 | § * 2]",
    ),
    (Variable, "comprehension", "RETURN [§ IN [1, 2, 3] | §]"),
    (
        Variable,
        "comprehension",
        "RETURN any(§ IN [1, 2] WHERE § > 1), all(§ IN [1, 2] WHERE § > 0)",
    ),
    (
        Variable,
        "comprehension",
        "MATCH (a) RETURN [(a)-->(§:Other) | §.n] ORDER BY a.n",
    ),
    (Variable, "unwind", "UNWIND [1, 2] AS § RETURN §"),
    (Variable, "with", "WITH 1 AS § RETURN § + 1"),
    (
        Variable,
        "with",
        "MATCH (§) WITH § WHERE §.n > 1 RETURN §.n ORDER BY §.n",
    ),
    (
        Variable,
        "with",
        "MATCH (§) WITH collect(§.n) AS ns RETURN ns",
    ),
    (Variable, "set", "MATCH (§ {n: 3}) SET §.m = 1 RETURN §.m"),
    (
        Variable,
        "let",
        "MATCH (n) LET § = n.n + 1 RETURN § ORDER BY n.n",
    ),
    (
        Variable,
        "filter",
        "MATCH (§) FILTER §.n > 1 RETURN §.n ORDER BY §.n",
    ),
    (
        Variable,
        "exists",
        "MATCH (§) WHERE EXISTS { MATCH (§)-->() } RETURN §.n ORDER BY §.n",
    ),
    (NodeLabel, "match", "MATCH (n:§) RETURN n.n ORDER BY n.n"),
    (NodeLabel, "match", "MATCH (n IS §) RETURN n.n ORDER BY n.n"),
    (NodeLabel, "match", "MATCH (n:§ {n: 1}) RETURN n.n"),
    (NodeLabel, "match", "MATCH (n:§:Other) RETURN n.n"),
    (
        NodeLabel,
        "match",
        "MATCH (is IS §) RETURN is.n ORDER BY is.n",
    ),
    (NodeLabel, "match", "MATCH (is:§) RETURN is.n ORDER BY is.n"),
    (NodeLabel, "match", "MATCH (IS §) RETURN count(*)"),
    (NodeLabel, "match", "MATCH ()-[IS §]->() RETURN count(*)"),
    (NodeLabel, "match", "MATCH ()-[is IS §]->() RETURN count(*)"),
    (
        NodeLabel,
        "where",
        "MATCH (n) WHERE n:§ RETURN n.n ORDER BY n.n",
    ),
    (
        NodeLabel,
        "where",
        "MATCH (n) WHERE n IS LABELED § RETURN n.n ORDER BY n.n",
    ),
    (
        NodeLabel,
        "where",
        "MATCH (n) WHERE n IS NOT LABELED § RETURN n.n ORDER BY n.n",
    ),
    (
        NodeLabel,
        "return",
        "MATCH (n) RETURN n:§, n IS LABELED § ORDER BY n.n",
    ),
    (NodeLabel, "set", "MATCH (n {n: 3}) SET n:§ RETURN n:§"),
    (
        NodeLabel,
        "set",
        "MATCH (n {n: 3}) SET n IS § RETURN n IS LABELED §",
    ),
    (
        NodeLabel,
        "set",
        "MATCH (n {n: 3}) SET n:§:Third RETURN n:§ AND n:Third",
    ),
    (
        NodeLabel,
        "set",
        "MATCH (n:§) REMOVE n:§ RETURN n.n ORDER BY n.n",
    ),
    (
        NodeLabel,
        "set",
        "MATCH (n:§) REMOVE n IS § RETURN n:§ ORDER BY n.n",
    ),
    (NodeLabel, "insert", "INSERT (:§ {n: 99})"),
    (
        NodeLabel,
        "insert",
        "MERGE (n:§ {n: 1}) ON MATCH SET n:§ RETURN n.n",
    ),
    (
        NodeLabel,
        "insert",
        "MERGE (n:§ {n: 5}) ON CREATE SET n IS § RETURN n.n",
    ),
    (NodeLabel, "insert", "MATCH (n:§ {n: 2}) DETACH DELETE n"),
    (
        RelationshipType,
        "match",
        "MATCH ()-[r:§]->() RETURN r.n ORDER BY r.n",
    ),
    (
        RelationshipType,
        "match",
        "MATCH ()-[r IS §]->() RETURN r.n ORDER BY r.n",
    ),
    (
        RelationshipType,
        "match",
        "MATCH ()-[:§|Other]->(m) RETURN m.n ORDER BY m.n",
    ),
    (
        RelationshipType,
        "match",
        "MATCH ()-[:§|:Other]->(m) RETURN m.n ORDER BY m.n",
    ),
    (
        RelationshipType,
        "match",
        "MATCH (a)-[:§*1..2]->(m) RETURN m.n ORDER BY m.n",
    ),
    (
        RelationshipType,
        "match",
        "MATCH (a)-[:§ {n: 10}]-(m) RETURN a.n, m.n ORDER BY a.n, m.n",
    ),
    (
        RelationshipType,
        "where",
        "MATCH ()-[r]->() WHERE r:§ RETURN r.n ORDER BY r.n",
    ),
    (
        RelationshipType,
        "where",
        "MATCH (a) WHERE (a)-[:§]->() RETURN a.n ORDER BY a.n",
    ),
    (
        RelationshipType,
        "return",
        "MATCH ()-[r]->() RETURN type(r) = '§', r IS LABELED § ORDER BY r.n",
    ),
    (
        RelationshipType,
        "comprehension",
        "MATCH (a:Other) RETURN [(a)-[:§]->(b) | b.n] ORDER BY a.n",
    ),
    (
        RelationshipType,
        "insert",
        "MATCH (a {n: 1}), (c {n: 3}) INSERT (a)-[:§ {n: 77}]->(c)",
    ),
    (PropertyKey, "match", "MATCH (n {§: 1}) RETURN n.n"),
    (PropertyKey, "match", "MATCH ()-[r {§: 10}]->() RETURN r.§"),
    (
        PropertyKey,
        "where",
        "MATCH (n) WHERE n.§ >= 2 RETURN n.n ORDER BY n.n",
    ),
    (
        PropertyKey,
        "where",
        "MATCH (n) WHERE n.§ IS NOT NULL RETURN n.n ORDER BY n.n",
    ),
    (PropertyKey, "return", "MATCH (n) RETURN n.§ ORDER BY n.§"),
    (
        PropertyKey,
        "return",
        "MATCH (n) RETURN n.§ + 1 AS v, n.§ IS NULL AS w ORDER BY n.n",
    ),
    (
        PropertyKey,
        "return",
        "MATCH (n) RETURN n.§ AS § ORDER BY n.n",
    ),
    (
        PropertyKey,
        "case",
        "MATCH (n) RETURN CASE WHEN n.§ > 1 THEN n.§ ELSE 0 END ORDER BY n.n",
    ),
    (PropertyKey, "map", "MATCH (n) RETURN {a: n.§} ORDER BY n.n"),
    (
        PropertyKey,
        "comprehension",
        "MATCH (n) WITH collect(n) AS ns RETURN [m IN ns WHERE m.§ > 1 | m.§]",
    ),
    (
        PropertyKey,
        "set",
        "MATCH (n {n: 3}) SET n.§ = 5 RETURN n.§",
    ),
    (
        PropertyKey,
        "set",
        "MATCH (n {n: 3}) SET n += {§: 5} RETURN n.§",
    ),
    (
        PropertyKey,
        "set",
        "MATCH (n:Other) REMOVE n.§ RETURN n.§, n.n ORDER BY n.n",
    ),
    (PropertyKey, "insert", "INSERT ({§: 1, n: 98})"),
    (
        PropertyKey,
        "order",
        "MATCH (n) RETURN n.n ORDER BY n.§ DESC, n.n",
    ),
    (MapKey, "map", "RETURN size(keys({§: 1}))"),
    (MapKey, "map", "RETURN {§: 1, b: 2}.§, {a: {§: 3}}.a.§"),
    (MapKey, "map", "RETURN {§: 1}['§'], size(keys({§: 1}))"),
    (
        MapKey,
        "map",
        "WITH {§: 1} AS m RETURN m.§, m['§'], '§' IN keys(m)",
    ),
    (
        MapKey,
        "case",
        "RETURN CASE WHEN true THEN {§: 1}.§ ELSE {§: 2}.§ END",
    ),
    (MapKey, "comprehension", "RETURN [x IN [1, 2] | {§: x}.§]"),
    (
        MapKey,
        "where",
        "MATCH (n) WHERE {§: n.n}.§ > 1 RETURN n.n ORDER BY n.n",
    ),
    (
        MapKey,
        "list",
        "RETURN [m IN [{§: 1}, {§: 2, b: [{§: 3}]}] | m.§]",
    ),
    (Alias, "return", "MATCH (n) RETURN n.n AS § ORDER BY n.n"),
    (Alias, "return", "MATCH (n) RETURN n.n AS § ORDER BY §"),
    (
        Alias,
        "return",
        "MATCH (n) RETURN DISTINCT n.n AS § ORDER BY n.n",
    ),
    (
        Alias,
        "return",
        "MATCH (n) RETURN count(n) AS §, 1 AS other",
    ),
    (Alias, "with", "MATCH (n) WITH n.n AS § RETURN § ORDER BY §"),
    (
        Alias,
        "with",
        "MATCH (n) WITH n.n AS § WHERE § > 1 RETURN § ORDER BY §",
    ),
    (
        Alias,
        "with",
        "MATCH (n) WITH n.n AS § RETURN 1 AS x ORDER BY x",
    ),
    (Alias, "unwind", "UNWIND [1, 2] AS § RETURN § * 2"),
    (
        Alias,
        "case",
        "MATCH (n) RETURN CASE WHEN n.n > 1 THEN 1 ELSE 0 END AS § ORDER BY n.n",
    ),
    (Alias, "map", "MATCH (n) RETURN {a: n.n} AS § ORDER BY n.n"),
    (Alias, "comprehension", "RETURN [x IN [1, 2] | x * 2] AS §"),
    (YieldName, "call", "CALL test.out() YIELD § RETURN §"),
    (YieldName, "call", "CALL test.out() YIELD § AS v RETURN v"),
    (YieldName, "call", "CALL test.out() YIELD out AS § RETURN §"),
    (YieldName, "call", "CALL test.out() YIELD § AS § RETURN §"),
    (
        YieldName,
        "call",
        "CALL test.out() YIELD § AS v WHERE v > 0 RETURN v",
    ),
    (ProcedureName, "call", "CALL §.p() YIELD out RETURN out"),
    (ProcedureName, "call", "CALL ns.§() YIELD out RETURN out"),
    (ProcedureName, "call", "CALL §.§() YIELD out AS v RETURN v"),
    (
        LabelExpression,
        "match",
        "MATCH (n:§|Other) RETURN n.n ORDER BY n.n",
    ),
    (
        LabelExpression,
        "match",
        "MATCH (n:Other|§) RETURN n.n ORDER BY n.n",
    ),
    (
        LabelExpression,
        "match",
        "MATCH (n IS §|Other) RETURN n.n ORDER BY n.n",
    ),
    (
        LabelExpression,
        "match",
        "MATCH (n:§&Other) RETURN n.n ORDER BY n.n",
    ),
    (
        LabelExpression,
        "match",
        "MATCH (n:Other&§) RETURN n.n ORDER BY n.n",
    ),
    (
        LabelExpression,
        "match",
        "MATCH (n IS §&!Other) RETURN n.n ORDER BY n.n",
    ),
    (
        LabelExpression,
        "match",
        "MATCH (n:!§) RETURN n.n ORDER BY n.n",
    ),
    (
        LabelExpression,
        "match",
        "MATCH (n IS !§) RETURN n.n ORDER BY n.n",
    ),
    (
        LabelExpression,
        "match",
        "MATCH (n:!(§|Other)) RETURN n.n ORDER BY n.n",
    ),
    (
        LabelExpression,
        "match",
        "MATCH (n:(§&Other)|%) RETURN n.n ORDER BY n.n",
    ),
    (
        LabelExpression,
        "match",
        "MATCH (n:%&§) RETURN n.n ORDER BY n.n",
    ),
    (
        LabelExpression,
        "match",
        "MATCH (n:§|:Other) RETURN n.n ORDER BY n.n",
    ),
    (
        LabelExpression,
        "match",
        "MATCH (is IS §|Other) RETURN is.n ORDER BY is.n",
    ),
    (
        LabelExpression,
        "match",
        "MATCH (IS §&Other) RETURN count(*)",
    ),
    (
        LabelExpression,
        "match",
        "MATCH ()-[r IS §|Other]->() RETURN r.n ORDER BY r.n",
    ),
    (
        LabelExpression,
        "match",
        "MATCH ()-[r:!§]->() RETURN r.n ORDER BY r.n",
    ),
    (
        LabelExpression,
        "where",
        "MATCH (n) WHERE n:§|Other RETURN n.n ORDER BY n.n",
    ),
    (
        LabelExpression,
        "where",
        "MATCH (n) WHERE n:§&Other OR n:!§ RETURN n.n ORDER BY n.n",
    ),
    (
        LabelExpression,
        "where",
        "MATCH (n) WHERE n IS LABELED §|Other RETURN n.n ORDER BY n.n",
    ),
    (
        LabelExpression,
        "where",
        "MATCH (n) WHERE n IS NOT LABELED §&Other RETURN n.n ORDER BY n.n",
    ),
    (
        LabelExpression,
        "case",
        "MATCH (n) RETURN CASE WHEN n:§|Other THEN 1 ELSE 0 END ORDER BY n.n",
    ),
    (
        LabelExpression,
        "case",
        "MATCH (n) RETURN CASE n:§&Other WHEN true THEN 1 ELSE 0 END ORDER BY n.n",
    ),
    (
        LabelExpression,
        "map",
        "MATCH (n) RETURN {k: n:§|Other} ORDER BY n.n",
    ),
    (
        LabelExpression,
        "list",
        "MATCH (n) RETURN [n:!§, n:§|Other] ORDER BY n.n",
    ),
    (
        LabelExpression,
        "comprehension",
        "MATCH (n) WITH collect(n) AS ns RETURN [m IN ns WHERE m:§&Other | m.n]",
    ),
    (
        LabelExpression,
        "comprehension",
        "MATCH (n) WITH collect(n) AS ns RETURN [m IN ns WHERE m:!§ | m.n]",
    ),
    (
        LabelExpression,
        "comprehension",
        "MATCH (n) WITH collect(n) AS ns RETURN [m IN ns WHERE (m:§|Other) | m.n]",
    ),
    (
        LabelExpression,
        "comprehension",
        "MATCH (n) WITH collect(n) AS ns RETURN [m IN ns WHERE coalesce(m:§|Other, false) | m.n]",
    ),
    (
        LabelExpression,
        "comprehension",
        "MATCH (a) RETURN [(a)-[:§|Other]->(b) | b.n] ORDER BY a.n",
    ),
    (
        LabelExpression,
        "comprehension",
        "MATCH (n) WITH collect(n) AS ns RETURN [m IN ns WHERE CASE WHEN m:§|Other THEN true END | m.n]",
    ),
    (
        LabelExpression,
        "comprehension",
        "MATCH (n) WITH collect(n) AS ns RETURN [m IN ns WHERE {k: m:§|Other}.k | m.n]",
    ),
    (
        LabelExpression,
        "comprehension",
        "MATCH (n) WITH collect(n) AS ns RETURN [m IN ns WHERE ns[CASE WHEN m:§|Other THEN 0 ELSE 1 END] IS NOT NULL | m.n]",
    ),
    (
        LabelExpression,
        "set",
        "MATCH (n {n: 3}) SET n:§&Third RETURN n:§&Third",
    ),
    (
        LabelExpression,
        "set",
        "MATCH (n {n: 3}) SET n IS §&Third RETURN n IS LABELED §&Third",
    ),
    (LabelExpression, "insert", "INSERT (:§&Other {n: 99})"),
    (
        LabelExpression,
        "exists",
        "MATCH (n) WHERE EXISTS { MATCH (n:§|Other)-->() } RETURN n.n ORDER BY n.n",
    ),
];

type Template = (Slot, &'static str, &'static str);
type Outcome = Result<Vec<String>, ErrorKind>;

const NOT_READABLE_AS_VARIABLE: &[&str] = &[
    "AND", "AS", "CASE", "CREATE", "DELETE", "DETACH", "ELSE", "END", "INSERT", "LIMIT", "MATCH",
    "MERGE", "OFFSET", "OPTIONAL", "OR", "ORDER", "REMOVE", "RETURN", "SET", "SKIP", "THEN",
    "UNION", "UNWIND", "WHEN", "WHERE", "WITH", "XOR",
];

const READ_AS_ANOTHER_EXPRESSION: &[&str] = &["DISTINCT", "FALSE", "NOT", "NULL", "TRUE"];

const NOT_A_LABEL_AFTER_IS_IN_SET: &[&str] = &["LABELED", "NOT", "NULL"];

const LOWER: u64 = 0;
const UPPER: u64 = u64::MAX;
const ALTERNATING: u64 = 0x5555_5555_5555_5555;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Expectation {
    SamePlainResult,
    SyntaxError,
    Unspecified,
}

fn binds_and_reads_back(template: &Template) -> bool {
    matches!(template.0, Variable | Alias | YieldName) && template.2.matches('§').count() >= 2
}

fn sets_labels_after_is(template: &Template) -> bool {
    template.2.contains("SET n IS §") || template.2.contains("REMOVE n IS §")
}

fn expectation(template: &Template, keyword: &str) -> Expectation {
    if binds_and_reads_back(template) {
        if READ_AS_ANOTHER_EXPRESSION.contains(&keyword) {
            Expectation::Unspecified
        } else if NOT_READABLE_AS_VARIABLE.contains(&keyword) {
            Expectation::SyntaxError
        } else {
            Expectation::SamePlainResult
        }
    } else if sets_labels_after_is(template) && NOT_A_LABEL_AFTER_IS_IN_SET.contains(&keyword) {
        Expectation::SyntaxError
    } else {
        Expectation::SamePlainResult
    }
}

fn graph(identifier: &str) -> Database {
    let db = db();
    for name in [
        "test.out".to_owned(),
        format!("{identifier}.p"),
        format!("ns.{identifier}"),
        format!("{identifier}.{identifier}"),
    ] {
        db.register_procedure(
            Procedure::new(name, |_| Ok(vec![vec![int(5), int(6)]].into()))
                .output("out")
                .output(identifier),
        )
        .unwrap();
    }
    let quoted = format!("`{identifier}`");
    run(
        &db,
        &format!(
            "INSERT (a:{quoted}:Other {{{quoted}: 1, n: 1}}), (b:{quoted} {{{quoted}: 2, n: 2}}), \
             (c:Other {{{quoted}: 3, n: 3}}), (a)-[:{quoted} {{{quoted}: 10, n: 10}}]->(b), \
             (b)-[:Other {{n: 20}}]->(c)"
        ),
    );
    db
}

fn render(template: &Template, identifier: &str) -> String {
    template.2.replace('§', identifier)
}

fn outcome(db: &Database, query: &str) -> Outcome {
    db.execute(query, &Parameters::new())
        .map(|result| {
            let mut rows: Vec<String> =
                result.rows().iter().map(|row| format!("{row:?}")).collect();
            rows.sort();
            rows
        })
        .map_err(|error| error.kind())
}

fn casing(keyword: &str, mask: u64) -> String {
    keyword
        .chars()
        .enumerate()
        .map(|(index, letter)| {
            if mask >> (index % 64) & 1 == 1 {
                letter.to_ascii_uppercase()
            } else {
                letter.to_ascii_lowercase()
            }
        })
        .collect()
}

fn writes(template: &Template) -> bool {
    ["SET ", "INSERT", "REMOVE", "MERGE", "DELETE"]
        .iter()
        .any(|word| template.2.contains(word))
}

static BASELINES: LazyLock<Vec<Outcome>> = LazyLock::new(|| {
    TEMPLATES
        .iter()
        .map(|template| outcome(&graph(PLAIN), &render(template, PLAIN)))
        .collect()
});

fn violations(keyword: &str, mask: u64, templates: &[usize]) -> Vec<String> {
    let identifier = casing(keyword, mask);
    let shared = graph(&identifier);
    let mut found = Vec::new();
    for &index in templates {
        let template = &TEMPLATES[index];
        let spelled = render(template, &identifier);
        let fresh;
        let db = if writes(template) {
            fresh = graph(&identifier);
            &fresh
        } else {
            &shared
        };
        let actual = outcome(db, &spelled);
        let expected = expectation(template, keyword);
        let holds = match expected {
            Expectation::SamePlainResult => BASELINES[index].is_ok() && actual == BASELINES[index],
            Expectation::SyntaxError => matches!(actual, Err(ErrorKind::Syntax(_))),
            Expectation::Unspecified => true,
        };
        if !holds {
            found.push(format!(
                "{:?} in {} context, {spelled}: {actual:?}, expected {expected:?} with plain {:?}",
                template.0, template.1, BASELINES[index]
            ));
        }
    }
    found
}

#[test]
fn the_plain_identifier_runs_every_template() {
    for (template, outcome) in TEMPLATES.iter().zip(BASELINES.iter()) {
        assert!(outcome.is_ok(), "{}: {outcome:?}", render(template, PLAIN));
    }
}

#[test]
fn every_keyword_works_in_every_slot_and_context_unless_it_is_reserved_there() {
    let mut found = Vec::new();
    for keyword in KEYWORDS {
        for mask in [LOWER, UPPER, ALTERNATING] {
            let indices: Vec<usize> = (0..TEMPLATES.len())
                .filter(|index| {
                    let rotation = [LOWER, UPPER, ALTERNATING];
                    rotation[(index + keyword.len()) % rotation.len()] == mask
                })
                .collect();
            found.extend(violations(keyword, mask, &indices));
        }
    }
    assert!(
        found.is_empty(),
        "{} violations:\n{}",
        found.len(),
        found.join("\n")
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    #[ignore = "left out of mise run check, run by mise run random"]
    fn a_keyword_in_any_letter_case_behaves_as_in_any_slot_and_context(
        keyword in 0..KEYWORDS.len(),
        template in 0..TEMPLATES.len(),
        mask in any::<u64>(),
    ) {
        let found = violations(KEYWORDS[keyword], mask, &[template]);
        prop_assert!(found.is_empty(), "{}", found.join("\n"));
    }
}
