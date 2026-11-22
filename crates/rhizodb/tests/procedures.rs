mod support;

use std::error::Error as _;

use rhizodb::Database;
use rhizodb::ErrorKind;
use rhizodb::Parameters;
use rhizodb::Procedure;
use rhizodb::Value;
use rhizodb::ValueType;
use support::*;

fn with_procedures() -> Database {
    let mut db = db();
    db.register_procedure(
        Procedure::new("test.split", |args| {
            let Some(Value::String(text)) = args.first() else {
                return Ok(Vec::new());
            };
            Ok(text
                .split(',')
                .enumerate()
                .map(|(index, part)| vec![string(part), int(index as i64)])
                .collect())
        })
        .parameter("text", ValueType::String)
        .output("part")
        .output("position"),
    )
    .unwrap();
    db.register_procedure(
        Procedure::new("test.echo", |args| Ok(vec![args.to_vec()]))
            .parameter("value", ValueType::Float)
            .output("value"),
    )
    .unwrap();
    db.register_procedure(Procedure::new("test.nothing", |_| Ok(vec![Vec::new()])))
        .unwrap();
    db.register_procedure(
        Procedure::new("test.fail", |_| Err("the body gave up".into())).output("never"),
    )
    .unwrap();
    db.register_procedure(
        Procedure::new("test.ragged", |_| Ok(vec![vec![int(1), int(2)]])).output("one"),
    )
    .unwrap();
    db
}

#[test]
fn yielded_outputs_become_variables_once_per_result_row() {
    let db = with_procedures();
    let rows = run(
        &db,
        "UNWIND ['a,b', 'c'] AS text CALL test.split(text) YIELD part AS p, position \
         WHERE position < 1 RETURN text, p",
    );
    assert_eq!(
        rows,
        [
            vec![string("a,b"), string("a")],
            vec![string("c"), string("c")]
        ]
    );
}

#[test]
fn a_query_that_is_one_call_returns_every_output_column() {
    let db = with_procedures();
    let result = db
        .execute("CALL test.split('x,y')", &Parameters::new())
        .unwrap();
    assert_eq!(result.columns(), ["part", "position"]);
    assert_eq!(
        result.rows(),
        [vec![string("x"), int(0)], vec![string("y"), int(1)]]
    );
}

#[test]
fn a_query_that_is_one_call_reads_left_out_arguments_from_parameters() {
    let db = with_procedures();
    let mut parameters = Parameters::new();
    parameters.insert("text".to_owned(), string("p,q"));
    let result = db.execute("CALL test.split", &parameters).unwrap();
    assert_eq!(result.rows().len(), 2);
    assert_eq!(
        error_kind(&db, "CALL test.split"),
        ErrorKind::MissingParameter
    );
    assert_eq!(
        error_kind(
            &db,
            "UNWIND [1] AS x CALL test.split YIELD part RETURN part"
        ),
        ErrorKind::InvalidArgumentPassingMode
    );
}

#[test]
fn a_call_inside_exists_passes_its_arguments_and_names_its_yields() {
    let db = with_procedures();
    let rows = run(&db, "RETURN EXISTS { CALL test.split('a') } AS e");
    assert_eq!(rows, [vec![Value::Boolean(true)]]);
    let mut parameters = Parameters::new();
    parameters.insert("text".to_owned(), string("p,q"));
    let failure = db
        .execute("RETURN EXISTS { CALL test.split } AS e", &parameters)
        .unwrap_err();
    assert_eq!(failure.kind(), ErrorKind::InvalidArgumentPassingMode);
    assert_eq!(
        error_kind(&db, "RETURN EXISTS { CALL test.split('a') YIELD * } AS e"),
        ErrorKind::InvalidClauseComposition
    );
}

#[test]
fn a_procedure_without_outputs_keeps_each_row_once() {
    let db = with_procedures();
    let rows = run(&db, "UNWIND [1, 2] AS x CALL test.nothing() RETURN x");
    assert_eq!(ints(&rows), [1, 2]);
}

#[test]
fn an_integer_argument_to_a_float_parameter_arrives_as_a_float() {
    let db = with_procedures();
    assert_eq!(
        single(&db, "CALL test.echo(2) YIELD value RETURN value"),
        float(2.0)
    );
}

#[test]
fn argument_types_are_checked_on_the_query_text_and_on_data() {
    let db = with_procedures();
    assert_eq!(
        error_kind(
            &db,
            "MATCH (n:Missing) CALL test.split(1) YIELD part RETURN part"
        ),
        ErrorKind::InvalidArgumentType
    );
    assert_eq!(
        error_kind(
            &db,
            "UNWIND [1] AS x CALL test.split(x) YIELD part RETURN part"
        ),
        ErrorKind::InvalidArgumentValue
    );
}

#[test]
fn calls_that_do_not_fit_the_procedure_fail_before_running() {
    let db = with_procedures();
    assert_eq!(
        error_kind(&db, "CALL test.missing()"),
        ErrorKind::ProcedureNotFound
    );
    assert_eq!(
        error_kind(&db, "CALL test.split('a', 'b')"),
        ErrorKind::InvalidNumberOfArguments
    );
    assert_eq!(
        error_kind(&db, "CALL test.split('a') YIELD missing RETURN missing"),
        ErrorKind::UnknownProcedureOutput
    );
    assert_eq!(
        error_kind(
            &db,
            "CALL test.split('a') YIELD part, position AS part RETURN part"
        ),
        ErrorKind::VariableAlreadyBound
    );
}

#[test]
fn a_failing_body_fails_the_query_and_keeps_its_error_as_the_source() {
    let db = with_procedures();
    let error = db
        .execute("CALL test.fail()", &Parameters::new())
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::ProcedureFailed);
    assert_eq!(
        error.source().map(ToString::to_string).as_deref(),
        Some("the body gave up")
    );
    assert_eq!(
        error_kind(&db, "CALL test.ragged()"),
        ErrorKind::ProcedureFailed
    );
}

#[test]
fn registering_a_name_again_replaces_the_procedure() {
    let mut db = with_procedures();
    db.register_procedure(
        Procedure::new("test.nothing", |_| Ok(vec![vec![int(7)]])).output("seven"),
    )
    .unwrap();
    assert_eq!(single(&db, "CALL test.nothing()"), int(7));
}

#[test]
fn a_query_run_before_a_procedure_is_replaced_calls_the_new_one() {
    let mut db = with_procedures();
    assert!(run(&db, "CALL test.nothing()").is_empty());
    db.register_procedure(
        Procedure::new("test.nothing", |_| Ok(vec![vec![int(7)]])).output("seven"),
    )
    .unwrap();
    assert_eq!(single(&db, "CALL test.nothing()"), int(7));
}
