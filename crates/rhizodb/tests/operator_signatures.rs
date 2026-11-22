mod support;

use rhizodb::Database;
use rhizodb::ErrorKind;
use rhizodb::Parameters;
use support::*;

const MATCH: &str = "MATCH p = (a)-[r:T]->(b)";

const OPERATORS: [&str; 37] = [
    "-{}",
    "+{}",
    "NOT {}",
    "{} + 1",
    "1 + {}",
    "{} + 1.5",
    "{} + 'a'",
    "'a' + {}",
    "{} + [1]",
    "[1] + {}",
    "{} + {}",
    "{} - 1",
    "1 - {}",
    "{} * 1.5",
    "{} / 1",
    "{} % 1",
    "{} ^ 1",
    "{} AND true",
    "false OR {}",
    "{} XOR true",
    "1 IN {}",
    "{} IN [1]",
    "{} STARTS WITH 'a'",
    "'a' CONTAINS {}",
    "{} =~ 'a'",
    "'a' =~ {}",
    "{}[0]",
    "{}['a']",
    "[1][{}]",
    "{a: 1}[{}]",
    "a[{}]",
    "{}[0..1]",
    "[1][{}..]",
    "[1][..{}]",
    "{}.x",
    "{}:T",
    "{} = 1",
];

const PROBES: [&str; 10] = [
    "1", "1.5", "'a'", "true", "[1]", "{a: 1}", "a", "r", "p", "null",
];

fn graph() -> Database {
    let db = db();
    run(&db, "CREATE (:A)-[:T]->(:B)");
    db
}

fn rejects_the_type(db: &Database, query: &str) -> bool {
    matches!(
        db.execute(query, &Parameters::new())
            .err()
            .map(|error| error.kind()),
        Some(
            ErrorKind::InvalidArgumentType
                | ErrorKind::TypeMismatch
                | ErrorKind::PropertyAccessOnNonEntity
                | ErrorKind::ListElementAccessByNonInteger
                | ErrorKind::MapElementAccessByNonString
        )
    )
}

#[test]
fn an_operator_accepts_the_same_operand_types_in_the_query_text_and_in_data() {
    let empty = db();
    let populated = graph();
    let mut disagreements = Vec::new();
    for template in OPERATORS {
        for probe in PROBES {
            let expression = template.replace("{}", probe);
            let in_text = format!("{MATCH} RETURN {expression} AS x");
            let through_data = format!(
                "{MATCH} UNWIND [{probe}] AS v RETURN {} AS x",
                template.replace("{}", "v")
            );
            for query in [&in_text, &through_data] {
                if let Err(error) = populated.execute(query, &Parameters::new())
                    && matches!(error.kind(), ErrorKind::Syntax(_))
                {
                    panic!("{query}: {error}");
                }
            }
            if rejects_the_type(&empty, &in_text) != rejects_the_type(&populated, &through_data) {
                disagreements.push(expression);
            }
        }
    }
    assert!(
        disagreements.is_empty(),
        "text and data disagree on: {disagreements:#?}"
    );
}
