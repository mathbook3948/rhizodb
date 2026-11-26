use super::*;

const LOG: &str = "\
     Running unittests src/lib.rs (target/debug/deps/rhizodb-1)
random_queries_in_memory_match_the_reference runs with RHIZODB_TEST_SEED=99
test engine::tests::fine ... ok
test engine::tests::breaks ... FAILED

failures:

---- engine::tests::breaks stdout ----
engine::tests::breaks runs with RHIZODB_TEST_SEED=7

thread 'engine::tests::breaks' panicked at src/engine.test.rs:3:5:
seed 7 (replay with RHIZODB_TEST_SEED=7) lost a commit

failures:
    engine::tests::breaks

test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out
     Running tests/query_oracle.rs (target/debug/deps/query_oracle-2)
random_queries_in_memory_match_the_reference runs with RHIZODB_TEST_SEED=42\r
test random_queries_in_memory_match_the_reference ... FAILED\r
\r
failures:\r
\r
---- random_queries_in_memory_match_the_reference stdout ----\r
thread 'random_queries_in_memory_match_the_reference' panicked at tests/query_oracle.rs:9:9:\r
rows differ\r
\r
failures:\r
    random_queries_in_memory_match_the_reference\r
";

#[test]
fn finds_each_failing_test_with_its_target_seed_and_output() {
    let failures = parse(LOG);

    assert_eq!(failures.len(), 2);
    assert_eq!(failures[0].target, "unittests src/lib.rs");
    assert_eq!(failures[0].test, "engine::tests::breaks");
    assert_eq!(failures[0].seeds, vec!["7"]);
    assert!(
        failures[0]
            .output
            .iter()
            .any(|line| line.ends_with("lost a commit"))
    );
    assert_eq!(failures[1].target, "tests/query_oracle.rs");
    assert_eq!(failures[1].seeds, vec!["42", "99"]);
    assert!(failures[1].output.iter().all(|line| !line.ends_with('\r')));
}

#[test]
fn a_passing_log_has_no_failures() {
    assert!(parse("test a ... ok\n\ntest result: ok. 1 passed\n").is_empty());
}

#[test]
fn one_report_holds_every_job_a_test_failed_in() {
    let logs = vec![
        ("random-ubuntu-latest".to_owned(), LOG.to_owned()),
        ("random-windows-latest".to_owned(), LOG.to_owned()),
    ];

    let reports = reports("random", &logs);

    assert_eq!(reports.len(), 2);
    let report = reports
        .iter()
        .find(|report| report.title == "engine::tests::breaks fails on main")
        .unwrap();
    assert!(report.body.contains("### random-ubuntu-latest"));
    assert!(report.body.contains("### random-windows-latest"));
    assert!(report.body.contains("`RHIZODB_TEST_SEED=7`"));
}

#[test]
fn a_log_without_a_failing_test_reports_the_job_with_its_tail() {
    let logs = vec![(
        "fuzz".to_owned(),
        "building\nerror: crash in parse\n".to_owned(),
    )];

    let reports = reports("nightly", &logs);

    assert_eq!(reports[0].title, "nightly job fuzz fails on main");
    assert!(reports[0].body.contains("error: crash in parse"));
}

#[test]
fn a_run_without_logs_reports_the_workflow() {
    let reports = reports("random", &[]);

    assert_eq!(reports[0].title, "random fails on main");
}

#[test]
fn many_failing_tests_share_one_report() {
    let log: String = (0..6).map(|n| format!("test t{n} ... FAILED\n")).collect();

    let reports = reports("random", &[("random-macos-latest".to_owned(), log)]);

    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].title, "random: 6 tests fail on main");
    assert!(reports[0].body.contains("## t5"));
}
