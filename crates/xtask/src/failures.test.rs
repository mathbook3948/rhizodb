use super::*;

const LOG: &str =
    "     Running unittests src/lib.rs (target/debug/build/rhizodb/1a2b/out/rhizodb-1a2b)
random_queries_in_memory_match_the_reference runs with RHIZODB_TEST_SEED=99
test engine::tests::fine ... ok
test engine::tests::breaks ... FAILED

failures:

---- engine::tests::breaks stdout ----
engine::tests::breaks runs with RHIZODB_TEST_SEED=7
     Running a captured line that looks like a header

thread 'engine::tests::breaks' panicked at src/engine.test.rs:3:5:
seed 7 (replay with RHIZODB_TEST_SEED=7) lost a commit

failures:
    engine::tests::breaks

test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out
     Running tests/query_oracle.rs (target\\debug\\deps\\query_oracle-3c4d.exe)\r
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

const CRASH: &str = "     Running tests/resource_limits.rs (target/debug/deps/resource_limits-5e6f)
a_deep_query runs with RHIZODB_TEST_SEED=11

thread 'a_deep_query' has overflowed its stack
fatal runtime error: stack overflow, aborting
error: test failed, to rerun pass `-p rhizodb --test resource_limits`
";

fn failure<'a>(failures: &'a [Failure], test: &str) -> &'a Failure {
    failures
        .iter()
        .find(|failure| failure.test.as_deref() == Some(test))
        .unwrap()
}

#[test]
fn finds_each_failing_test_with_its_binary_seed_and_output() {
    let failures = parse(LOG);

    assert_eq!(failures.len(), 2);
    let breaks = failure(&failures, "engine::tests::breaks");
    assert_eq!(breaks.binary, "rhizodb");
    assert_eq!(breaks.seeds, vec!["7"]);
    assert!(
        breaks
            .output
            .iter()
            .any(|line| line.ends_with("lost a commit"))
    );
    let oracle = failure(&failures, "random_queries_in_memory_match_the_reference");
    assert_eq!(oracle.binary, "query_oracle");
    assert_eq!(oracle.seeds, vec!["42", "99"]);
    assert!(oracle.output.iter().all(|line| !line.ends_with('\r')));
}

#[test]
fn a_captured_line_that_looks_like_a_header_stays_in_the_output() {
    let failures = parse(LOG);

    let breaks = failure(&failures, "engine::tests::breaks");
    assert!(
        breaks
            .output
            .iter()
            .any(|line| line.contains("captured line that looks like a header"))
    );
}

#[test]
fn a_failing_doctest_names_the_doctests_of_its_crate() {
    let log = "   Doc-tests rhizodb\ntest src/lib.rs - guide (line 10) ... FAILED\n";

    let failures = parse(log);

    assert_eq!(failures[0].binary, "rhizodb doctests");
}

#[test]
fn a_binary_that_dies_without_a_failed_test_is_reported_with_the_lines_before_it() {
    let log = format!("{LOG}{CRASH}");

    let failures = parse(&log);

    assert_eq!(failures.len(), 3);
    let crashed = failures
        .iter()
        .find(|failure| failure.binary == "resource_limits")
        .unwrap();
    assert_eq!(crashed.test, None);
    assert_eq!(crashed.seeds, vec!["11"]);
    assert!(
        crashed
            .output
            .iter()
            .any(|line| line.contains("stack overflow"))
    );
}

#[test]
fn a_binary_whose_failed_tests_are_named_is_not_reported_again() {
    let log = format!("{LOG}error: test failed, to rerun pass `-p rhizodb --test query_oracle`\n");

    assert_eq!(parse(&log).len(), 2);
}

#[test]
fn a_passing_log_has_no_failures() {
    assert!(parse("test a ... ok\n\ntest result: ok. 1 passed\n").is_empty());
}

#[test]
fn one_report_holds_every_job_a_test_failed_in_and_names_its_binary() {
    let logs = vec![
        ("random-ubuntu-latest".to_owned(), LOG.to_owned()),
        ("random-windows-latest".to_owned(), LOG.to_owned()),
    ];

    let reports = reports("random", &logs);

    assert_eq!(reports.len(), 2);
    let report = reports
        .iter()
        .find(|report| report.title == "rhizodb: engine::tests::breaks fails on main")
        .unwrap();
    assert!(report.body.contains("### random-ubuntu-latest"));
    assert!(report.body.contains("### random-windows-latest"));
    assert!(report.body.contains("`RHIZODB_TEST_SEED=7`"));
}

#[test]
fn a_binary_that_dies_gets_a_title_of_its_own() {
    let reports = reports(
        "random",
        &[("random-ubuntu-latest".to_owned(), CRASH.to_owned())],
    );

    assert_eq!(
        reports[0].title,
        "resource_limits dies without a test result on main"
    );
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
fn many_failing_tests_share_one_report_whose_title_holds_no_count() {
    let log: String = (0..6).map(|n| format!("test t{n} ... FAILED\n")).collect();

    let reports = reports("random", &[("random-macos-latest".to_owned(), log)]);

    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].title, "random: several tests fail on main");
    assert!(reports[0].body.contains("## t5"));
}

#[test]
fn a_long_output_is_cut_and_says_how_much() {
    let mut log = String::from("---- huge stdout ----\n");
    for line in 0..5000 {
        log.push_str(&format!("output line {line} {}\n", "x".repeat(100)));
    }

    let reports = reports("random", &[("random-ubuntu-latest".to_owned(), log)]);

    assert!(reports[0].body.len() <= MAX_BODY);
    assert!(reports[0].body.contains("more lines of output were cut"));
}

#[test]
fn every_report_fits_an_issue_however_many_tests_fail() {
    let mut log = String::new();
    for test in 0..40 {
        log.push_str(&format!("---- t{test} stdout ----\n"));
        for line in 0..2000 {
            log.push_str(&format!("t{test} line {line} {}\n", "y".repeat(200)));
        }
        log.push_str("failures:\n");
    }
    let logs: Vec<(String, String)> = ["ubuntu", "macos", "windows"]
        .iter()
        .map(|os| (format!("random-{os}-latest"), log.clone()))
        .collect();

    let reports = reports("random", &logs);

    assert!(reports.iter().all(|report| report.body.len() <= MAX_BODY));
    assert!(
        reports
            .iter()
            .any(|report| report.body.contains("left out to fit one issue"))
    );
}
