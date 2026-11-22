use super::*;

fn step(text: &str) -> RawStep {
    RawStep {
        text: text.into(),
        ..RawStep::default()
    }
}

fn with_doc(text: &str, docstring: &str) -> RawStep {
    RawStep {
        docstring: Some(docstring.into()),
        ..step(text)
    }
}

fn with_table(text: &str, rows: &[&[&str]]) -> RawStep {
    RawStep {
        table: Some(
            rows.iter()
                .map(|row| row.iter().map(|cell| cell.to_string()).collect())
                .collect(),
        ),
        ..step(text)
    }
}

fn read_scenario(then: Vec<RawStep>) -> Vec<RawStep> {
    let mut steps = vec![
        step("an empty graph"),
        with_doc("executing query:", "MATCH (n) RETURN n"),
    ];
    steps.extend(then);
    steps
}

#[test]
fn compiles_graph_setup_parameters_query_rows_and_side_effects() {
    let steps = vec![
        step("the binary-tree-1 graph"),
        with_doc("having executed:", "CREATE ()"),
        with_table("parameters are:", &[&["p", "1"], &["q", "'x'"]]),
        with_doc("executing query:", "RETURN $p AS a"),
        with_table(
            "the result should be, in order:",
            &[&["a", "b"], &["1", "[2]"]],
        ),
        with_table(
            "the side effects should be:",
            &[&["+nodes", "2"], &["-properties", "1"]],
        ),
    ];

    let parts = compile_steps(&steps).expect("compiles");

    assert_eq!(parts.graph, GraphSetup::Named("binary-tree-1".into()));
    assert_eq!(parts.setup, vec!["CREATE ()"]);
    assert_eq!(
        parts.parameters,
        BTreeMap::from([
            ("p".to_string(), Value::Integer(1)),
            ("q".to_string(), Value::String("x".into())),
        ])
    );
    assert_eq!(parts.query, "RETURN $p AS a");
    assert_eq!(
        parts.expected,
        Expected::Rows {
            columns: vec!["a".into(), "b".into()],
            rows: vec![vec![
                Value::Integer(1),
                Value::List(vec![Value::Integer(2)])
            ]],
            ordering: Ordering::InOrder,
            lists: Lists::Strict,
        }
    );
    let effects = parts.side_effects.expect("asserted");
    assert_eq!(
        effects.added,
        Counts {
            nodes: 2,
            ..Counts::default()
        }
    );
    assert_eq!(
        effects.removed,
        Counts {
            properties: 1,
            ..Counts::default()
        }
    );
}

#[test]
fn the_four_result_steps_pick_ordering_and_list_mode() {
    let cases = [
        (
            "the result should be, in any order:",
            Ordering::AnyOrder,
            Lists::Strict,
        ),
        (
            "the result should be, in order:",
            Ordering::InOrder,
            Lists::Strict,
        ),
        (
            "the result should be (ignoring element order for lists):",
            Ordering::AnyOrder,
            Lists::IgnoreElementOrder,
        ),
        (
            "the result should be, in order (ignoring element order for lists):",
            Ordering::InOrder,
            Lists::IgnoreElementOrder,
        ),
    ];
    for (text, ordering, lists) in cases {
        let parts =
            compile_steps(&read_scenario(vec![with_table(text, &[&["n"]])])).expect("compiles");

        assert_eq!(
            parts.expected,
            Expected::Rows {
                columns: vec!["n".into()],
                rows: vec![],
                ordering,
                lists
            },
            "{text}"
        );
    }
}

#[test]
fn no_side_effects_is_an_assertion_and_absence_is_not() {
    let asserted = compile_steps(&read_scenario(vec![
        step("the result should be empty"),
        step("no side effects"),
    ]))
    .expect("compiles");
    let absent =
        compile_steps(&read_scenario(vec![step("the result should be empty")])).expect("compiles");

    assert_eq!(asserted.side_effects, Some(SideEffects::default()));
    assert_eq!(absent.side_effects, None);
}

#[test]
fn an_expected_error_implies_no_side_effects_unless_stated() {
    let implied = compile_steps(&read_scenario(vec![step(
        "a SyntaxError should be raised at compile time: UndefinedVariable",
    )]))
    .expect("compiles");
    let stated = compile_steps(&read_scenario(vec![
        step("an EntityNotFound should be raised at runtime: DeletedEntityAccess"),
        with_table("the side effects should be:", &[&["+nodes", "1"]]),
    ]))
    .expect("compiles");

    assert_eq!(
        implied.expected,
        Expected::Error {
            kind: "SyntaxError".into(),
            phase: Phase::CompileTime,
            detail: "UndefinedVariable".into()
        }
    );
    assert_eq!(implied.side_effects, Some(SideEffects::default()));
    assert_eq!(
        stated.expected,
        Expected::Error {
            kind: "EntityNotFound".into(),
            phase: Phase::Runtime,
            detail: "DeletedEntityAccess".into()
        }
    );
    assert_eq!(stated.side_effects.expect("stated").added.nodes, 1);
}

#[test]
fn parses_every_error_phase() {
    for (text, phase) in [
        ("compile time", Phase::CompileTime),
        ("runtime", Phase::Runtime),
        ("any time", Phase::AnyTime),
    ] {
        let step_text = format!("a TypeError should be raised at {text}: InvalidArgumentType");
        let parts = compile_steps(&read_scenario(vec![step(&step_text)])).expect("compiles");

        assert!(matches!(parts.expected, Expected::Error { phase: p, .. } if p == phase));
    }
}

#[test]
fn a_control_query_gets_its_own_expectation() {
    let parts = compile_steps(&read_scenario(vec![
        step("the result should be empty"),
        step("no side effects"),
        with_doc("executing control query:", "MATCH (n) RETURN n.k"),
        with_table("the result should be, in any order:", &[&["n.k"], &["1"]]),
    ]))
    .expect("compiles");

    assert_eq!(parts.expected, Expected::Empty);
    let control = parts.control.expect("control query");
    assert_eq!(control.query, "MATCH (n) RETURN n.k");
    assert_eq!(
        control.expected,
        Expected::Rows {
            columns: vec!["n.k".into()],
            rows: vec![vec![Value::Integer(1)]],
            ordering: Ordering::AnyOrder,
            lists: Lists::Strict,
        }
    );
}

#[test]
fn a_procedure_keeps_its_signature_and_typed_rows() {
    let steps = vec![
        step("any graph"),
        with_table(
            "there exists a procedure test.my.proc(in :: INTEGER?) :: (out :: STRING?):",
            &[&["in", "out"], &["1", "'a'"]],
        ),
        with_doc("executing query:", "CALL test.my.proc(1)"),
        step("the result should be empty"),
    ];

    let parts = compile_steps(&steps).expect("compiles");

    assert_eq!(
        parts.procedures,
        vec![Procedure {
            signature: "test.my.proc(in :: INTEGER?) :: (out :: STRING?)".into(),
            columns: vec!["in".into(), "out".into()],
            rows: vec![vec![Value::Integer(1), Value::String("a".into())]],
        }]
    );
}

#[test]
fn an_unknown_step_is_an_error_naming_the_line() {
    let mut unknown = step("the moon is full");
    unknown.line = 12;

    let error = compile_steps(&read_scenario(vec![unknown])).expect_err("unknown step");

    assert!(error.contains("line 12"), "{error}");
    assert!(error.contains("unrecognized step"), "{error}");
}

#[test]
fn an_unparsable_cell_is_an_error() {
    let error = compile_steps(&read_scenario(vec![with_table(
        "the result should be, in any order:",
        &[&["n"], &["(:A"]],
    )]))
    .expect_err("bad cell");

    assert!(error.contains("(:A"), "{error}");
}

#[test]
fn malformed_scenarios_are_errors() {
    let no_expectation = read_scenario(vec![]);
    let no_query = vec![step("an empty graph"), step("the result should be empty")];
    let no_graph = vec![
        with_doc("executing query:", "RETURN 1"),
        step("the result should be empty"),
    ];
    let bad_effect = read_scenario(vec![
        step("the result should be empty"),
        with_table("the side effects should be:", &[&["+widgets", "1"]]),
    ]);
    let ragged_row = read_scenario(vec![with_table(
        "the result should be, in any order:",
        &[&["a", "b"], &["1"]],
    )]);
    let setup_after_query = vec![
        step("an empty graph"),
        with_doc("executing query:", "RETURN 1"),
        with_doc("having executed:", "CREATE ()"),
    ];
    let missing_doc = vec![step("an empty graph"), step("executing query:")];

    for (name, steps) in [
        ("no expectation", no_expectation),
        ("no query", no_query),
        ("no graph", no_graph),
        ("bad side effect", bad_effect),
        ("ragged row", ragged_row),
        ("setup after query", setup_after_query),
        ("missing doc string", missing_doc),
    ] {
        assert!(compile_steps(&steps).is_err(), "{name} should be rejected");
    }
}
