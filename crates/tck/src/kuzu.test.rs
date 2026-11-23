use super::*;

const FILE: &str = "-DATASET CSV empty
--

-CASE First
-LOG a log line
-STATEMENT CREATE NODE TABLE t(id INT64, PRIMARY KEY(id));
---- ok
-STATEMENT UNWIND [1, 2] AS x
           CREATE (:t {id: x})
---- ok
-RELOADDB
-CHECK_ORDER
-STATEMENT MATCH (n:t) RETURN n.id, n.id > 1 ORDER BY n.id;
---- 2
1|False
2|True
-STATEMENT RETURN 1 / 0;
---- error
Runtime exception: division by zero.
-STATEMENT CALL x() RETURN *;
---- hash
2 tuples hashing to abc

-CASE Second
-SKIP
-CREATE_CONNECTION conn1
-STATEMENT [conn1] RETURN 1;
---- 1
1
";

#[test]
fn a_file_parses_into_cases_with_steps() {
    let cases = parse_file("dir/file.test", FILE).unwrap();
    assert_eq!(cases.len(), 2);
    assert_eq!(cases[0].name, "dir/file.test::First");
    assert_eq!(cases[0].unsupported, None);
    assert_eq!(
        cases[0].steps,
        [
            Step::Statement {
                query: "CREATE NODE TABLE t(id INT64, PRIMARY KEY(id));".to_owned(),
                expect: Expect::Ok,
                ordered: false,
            },
            Step::Statement {
                query: "UNWIND [1, 2] AS x\nCREATE (:t {id: x})".to_owned(),
                expect: Expect::Ok,
                ordered: false,
            },
            Step::Reload,
            Step::Statement {
                query: "MATCH (n:t) RETURN n.id, n.id > 1 ORDER BY n.id;".to_owned(),
                expect: Expect::Rows(vec!["1|False".to_owned(), "2|True".to_owned()]),
                ordered: true,
            },
            Step::Statement {
                query: "RETURN 1 / 0;".to_owned(),
                expect: Expect::Error,
                ordered: false,
            },
            Step::Statement {
                query: "CALL x() RETURN *;".to_owned(),
                expect: Expect::Hash,
                ordered: false,
            },
        ]
    );
    assert!(cases[1].skipped);
    assert_eq!(
        cases[1].unsupported.as_deref(),
        Some("the directive -CREATE_CONNECTION")
    );
}

#[test]
fn a_case_passes_across_a_reload_and_fails_on_a_wrong_row() {
    let cases = parse_file("dir/file.test", FILE).unwrap();
    let mut first = cases[0].clone();
    first.steps.pop();
    assert_eq!(evaluate(&first), Verdict::Passed);
    if let Some(Step::Statement { expect, .. }) = first.steps.get_mut(3) {
        *expect = Expect::Rows(vec!["1|False".to_owned(), "3|True".to_owned()]);
    }
    assert!(matches!(evaluate(&first), Verdict::Failed(_)));
}

#[test]
fn a_rolled_back_transaction_leaves_nothing_after_a_reload() {
    let text = "-DATASET CSV empty
-CASE Rollback
-STATEMENT BEGIN TRANSACTION;
---- ok
-STATEMENT CREATE (:t {id: 1});
---- ok
-STATEMENT ROLLBACK;
---- ok
-RELOADDB
-STATEMENT MATCH (n:t) RETURN count(n);
---- 1
0
";
    let cases = parse_file("rollback.test", text).unwrap();
    assert_eq!(evaluate(&cases[0]), Verdict::Passed);
}

#[test]
fn values_print_as_kuzu_prints_them() {
    let list = Value::List(vec![
        Value::Integer(1),
        Value::Float(2.5),
        Value::Null,
        Value::String("a".to_owned()),
    ]);
    assert_eq!(kuzu_text(&list), "[1,2.500000,,a]");
    assert_eq!(kuzu_text(&Value::Boolean(false)), "False");
}

#[test]
fn a_file_of_another_dataset_is_refused() {
    assert!(parse_file("x.test", "-DATASET CSV tinysnb\n-CASE A\n").is_err());
}

#[test]
fn a_primary_key_names_its_table_and_column() {
    assert_eq!(
        primary_key("CREATE NODE TABLE Person(id INT64, name STRING, PRIMARY KEY (id));"),
        Some(("Person".to_owned(), "id".to_owned()))
    );
    assert_eq!(
        primary_key("CREATE REL TABLE knows(FROM Person TO Person)"),
        None
    );
}
