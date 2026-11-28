use super::any_expr;
use crate::gql::ast::Expr;
use crate::gql::ast::Literal;
use crate::gql::testing::expr;

fn finds_marker(source: &str) -> bool {
    any_expr(
        &expr(source),
        &mut |found| matches!(found, Expr::Literal(Literal::String(text)) if text == "marker"),
    )
}

#[test]
fn every_place_an_expression_can_sit_is_walked() {
    for source in [
        "'marker'",
        "[1, 'marker']",
        "{a: 'marker'}",
        "'marker'.a",
        "[1][0]['marker']",
        "[1][0..'marker']",
        "[1]['marker'..]",
        "'marker':A",
        "NOT 'marker'",
        "1 + 'marker'",
        "1 < 2 <= 'marker'",
        "'marker' IS NULL",
        "f('marker')",
        "CASE 'marker' WHEN 1 THEN 2 END",
        "CASE 1 WHEN 'marker' THEN 2 END",
        "CASE 1 WHEN < 'marker' THEN 2 END",
        "CASE WHEN true THEN 'marker' END",
        "CASE WHEN true THEN 1 ELSE 'marker' END",
        "[x IN 'marker' | x]",
        "[x IN [] WHERE 'marker']",
        "[x IN [] | 'marker']",
        "any(x IN [] WHERE 'marker')",
        "[(a)-->({k: 'marker'}) | 1]",
        "[(a)-[{k: 'marker'}]->() | 1]",
        "[(a)--() WHERE 'marker' | 1]",
        "[(a)--() | 'marker']",
        "EXISTS { ({k: 'marker'}) }",
        "EXISTS { (a) WHERE 'marker' }",
        "COUNT { MATCH (a) WHERE 'marker' RETURN a }",
        "COUNT { MATCH ({k: 'marker'}) RETURN 1 }",
        "COLLECT { UNWIND 'marker' AS x RETURN x }",
        "VALUE { WITH 'marker' AS x RETURN x }",
        "VALUE { WITH 1 AS x WHERE 'marker' RETURN x }",
        "VALUE { RETURN 1 ORDER BY 'marker' }",
        "VALUE { RETURN 1 SKIP 'marker' }",
        "VALUE { RETURN 1 LIMIT 'marker' }",
        "VALUE { FILTER 'marker' RETURN 1 }",
        "VALUE { LET x = 'marker' RETURN x }",
        "VALUE { CALL p('marker') YIELD x RETURN x }",
        "VALUE { CALL p() YIELD x WHERE 'marker' RETURN x }",
        "VALUE { CALL { RETURN 'marker' AS x } RETURN x }",
        "VALUE { CALL { RETURN 1 AS x UNION RETURN 'marker' AS x } RETURN x }",
        "VALUE { CALL { RETURN 1 AS y NEXT RETURN 'marker' AS x } RETURN x }",
        "VALUE { CREATE ({k: 'marker'}) RETURN 1 }",
        "VALUE { MERGE ({k: 'marker'}) RETURN 1 }",
        "VALUE { MATCH (a) MERGE (a)-[:R]->(b) ON CREATE SET b.k = 'marker' RETURN 1 }",
        "VALUE { MATCH (a) MERGE (a)-[:R]->(b) ON MATCH SET b.k = 'marker' RETURN 1 }",
        "VALUE { MATCH (a) SET a = {k: 'marker'} RETURN 1 }",
        "VALUE { MATCH (a) SET a += {k: 'marker'} RETURN 1 }",
        "VALUE { MATCH (a) REMOVE VALUE { RETURN 'marker' }.k RETURN 1 }",
        "VALUE { MATCH (a) DELETE 'marker' RETURN 1 }",
        "'marker' IS DISTINCT FROM 1",
        "1 IS DISTINCT FROM 'marker'",
        "CAST('marker' AS STRING)",
        "reduce(a = 'marker', x IN [] | a)",
        "reduce(a = 0, x IN 'marker' | a)",
        "reduce(a = 0, x IN [] | 'marker')",
    ] {
        assert!(finds_marker(source), "{source}");
    }
    for source in [
        "'other'",
        "[(a)--() | 1]",
        "VALUE { MATCH (a:marker) RETURN a }",
    ] {
        assert!(!finds_marker(source), "{source}");
    }
}
