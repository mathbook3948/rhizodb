mod support;

use rhizodb::Database;
use rhizodb::ErrorKind;
use rhizodb::Parameters;
use support::*;

const MATCH: &str = "MATCH p = (a)-[r:T]->(b)";

const FUNCTIONS: [(&str, &[&str]); 53] = [
    ("id", &["a"]),
    ("labels", &["a"]),
    ("type", &["r"]),
    ("keys", &["a"]),
    ("properties", &["a"]),
    ("size", &["[1]"]),
    ("length", &["p"]),
    ("head", &["[1]"]),
    ("last", &["[1]"]),
    ("tail", &["[1]"]),
    ("range", &["1", "5", "1"]),
    ("reverse", &["[1]"]),
    ("coalesce", &["1"]),
    ("toInteger", &["1"]),
    ("toFloat", &["1"]),
    ("toString", &["1"]),
    ("toBoolean", &["true"]),
    ("abs", &["1"]),
    ("sign", &["1"]),
    ("ceil", &["1"]),
    ("floor", &["1"]),
    ("round", &["1"]),
    ("sqrt", &["1"]),
    ("exp", &["1"]),
    ("log", &["1"]),
    ("log10", &["1"]),
    ("sin", &["1"]),
    ("cos", &["1"]),
    ("tan", &["1"]),
    ("asin", &["1"]),
    ("acos", &["1"]),
    ("atan", &["1"]),
    ("atan2", &["1", "1"]),
    ("degrees", &["1"]),
    ("radians", &["1"]),
    ("pi", &[]),
    ("e", &[]),
    ("rand", &[]),
    ("startNode", &["r"]),
    ("endNode", &["r"]),
    ("nodes", &["p"]),
    ("relationships", &["p"]),
    ("toUpper", &["'a'"]),
    ("toLower", &["'a'"]),
    ("trim", &["'a'"]),
    ("lTrim", &["'a'"]),
    ("rTrim", &["'a'"]),
    ("substring", &["'abc'", "1", "1"]),
    ("replace", &["'abc'", "'b'", "'x'"]),
    ("split", &["'a,b'", "','"]),
    ("left", &["'abc'", "1"]),
    ("right", &["'abc'", "1"]),
    ("exists", &["1"]),
];

const AGGREGATES: [(&str, &[&str]); 10] = [
    ("count", &["1"]),
    ("collect", &["1"]),
    ("min", &["1"]),
    ("max", &["1"]),
    ("sum", &["1"]),
    ("avg", &["1"]),
    ("stDev", &["1"]),
    ("stDevP", &["1"]),
    ("percentileCont", &["1", "0.5"]),
    ("percentileDisc", &["1", "0.5"]),
];

const PROBES: [&str; 10] = [
    "1", "1.5", "'a'", "true", "[1]", "{a: 1}", "a", "r", "p", "null",
];

fn graph() -> Database {
    let db = db();
    run(&db, "CREATE (:A)-[:T]->(:B)");
    db
}

fn call(name: &str, valid: &[&str], position: usize, argument: &str) -> String {
    let arguments: Vec<&str> = valid
        .iter()
        .enumerate()
        .map(|(index, text)| if index == position { argument } else { text })
        .collect();
    format!("{name}({})", arguments.join(", "))
}

fn rejects_the_type(db: &Database, query: &str) -> bool {
    matches!(
        db.execute(query, &Parameters::new())
            .err()
            .map(|error| error.kind()),
        Some(
            ErrorKind::InvalidArgumentType
                | ErrorKind::InvalidArgumentValue
                | ErrorKind::TypeMismatch
        )
    )
}

#[test]
fn a_function_accepts_the_same_argument_types_in_the_query_text_and_in_data() {
    let empty = db();
    let populated = graph();
    let mut disagreements = Vec::new();
    for (name, valid) in FUNCTIONS.into_iter().chain(AGGREGATES) {
        for position in 0..valid.len() {
            for probe in PROBES {
                let expression = call(name, valid, position, probe);
                let in_text = format!("{MATCH} RETURN {expression}");
                let through_data = format!(
                    "{MATCH} UNWIND [{probe}] AS v RETURN {}",
                    call(name, valid, position, "v")
                );
                if rejects_the_type(&empty, &in_text) != rejects_the_type(&populated, &through_data)
                {
                    disagreements.push(expression);
                }
            }
        }
    }
    assert!(
        disagreements.is_empty(),
        "text and data disagree on: {disagreements:#?}"
    );
}

#[test]
fn length_takes_a_path_and_size_takes_a_list_or_string() {
    let db = graph();
    assert_eq!(single(&db, &format!("{MATCH} RETURN length(p)")), int(1));
    assert_eq!(single(&db, "RETURN size([1, 2])"), int(2));
    assert_eq!(single(&db, "RETURN size('abc')"), int(3));
    for query in [
        "RETURN length('abc')",
        "RETURN length([1, 2])",
        "RETURN size({a: 1})",
    ] {
        assert_eq!(
            error_kind(&db, query),
            ErrorKind::InvalidArgumentType,
            "{query}"
        );
    }
    for query in [
        "WITH 'abc' AS s RETURN length(s)",
        "WITH [1, 2] AS l RETURN length(l)",
        "WITH {a: 1} AS m RETURN size(m)",
    ] {
        assert_eq!(
            error_kind(&db, query),
            ErrorKind::InvalidArgumentValue,
            "{query}"
        );
    }
}
