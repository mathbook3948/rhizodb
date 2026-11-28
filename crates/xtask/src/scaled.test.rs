use super::*;

const MISE: &str = r#"[tasks.random]
run = "cargo test --workspace -- --ignored"

[tasks.storage-deep]
env = { RHIZODB_TEST_SCALE = "100" }
run = "cargo test --release --package rhizodb --lib -- storage:: graph::meta --include-ignored --skip engine::bench_tests::"

[tasks.random-deep]
run = [
  "cargo test --release --package rhizodb --test isolation --test backup -- --include-ignored",
  { task = "parallel-deep" },
]

[tasks.parallel-deep]
run = "cargo test --release --package rhizodb --tests -- --include-ignored"
"#;

fn lib() -> Selection<'static> {
    selection(&task_commands(MISE, "storage-deep").unwrap())
}

fn integration() -> Selection<'static> {
    selection(&task_commands(MISE, "random-deep").unwrap())
}

fn problems(sources: &[Source]) -> Vec<String> {
    unscaled_problems(sources, &lib(), &integration())
}

#[test]
fn reads_the_cargo_test_commands_of_one_task_only() {
    assert_eq!(
        task_commands(MISE, "random-deep").unwrap(),
        vec!["--release --package rhizodb --test isolation --test backup -- --include-ignored"]
    );
    assert_eq!(task_commands(MISE, "missing"), None);
}

#[test]
fn splits_a_command_into_filters_skips_and_test_targets() {
    assert_eq!(
        lib(),
        Selection {
            filters: vec!["storage::", "graph::meta"],
            skips: vec!["engine::bench_tests::"],
            tests: vec![],
        }
    );
    assert_eq!(integration().tests, vec!["isolation", "backup"]);
}

#[test]
fn accepts_scaled_modules_and_tests_the_deep_tasks_select() {
    let sources = [
        Source {
            path: "crates/rhizodb/src/graph/meta.rs",
            text: "#[cfg(test)]\n#[path = \"meta.test.rs\"]\nmod tests;\n",
        },
        Source {
            path: "crates/rhizodb/src/graph/meta.test.rs",
            text: "crate::test_scale::cases(32)",
        },
        Source {
            path: "crates/rhizodb/src/storage/file.rs",
            text: "fn f() { crate::test_scale::cases(8); }",
        },
        Source {
            path: "crates/rhizodb/src/test_scale.rs",
            text: "pub(crate) fn cases(fast: u32) -> u32 { test_scale::x }",
        },
        Source {
            path: "crates/rhizodb/tests/backup.rs",
            text: "mod support;\nuse support::*;\nfn f() { for _ in 0..cases(5) {} }",
        },
        Source {
            path: "crates/rhizodb/tests/regex.rs",
            text: "proptest! { #![proptest_config(ProptestConfig::with_cases(64))] }",
        },
        Source {
            path: "crates/rhizodb/tests/support/mod.rs",
            text: "pub fn cases(fast: u64) -> u64 { std::env::var(\"RHIZODB_TEST_SCALE\") }",
        },
    ];

    assert_eq!(problems(&sources), Vec::<String>::new());
}

#[test]
fn reports_a_scaled_module_under_another_name_than_its_file() {
    let sources = [
        Source {
            path: "crates/rhizodb/src/graph.rs",
            text: "#[cfg(test)]\n#[path = \"graph/meta_crash.test.rs\"]\nmod crash_tests;\n",
        },
        Source {
            path: "crates/rhizodb/src/graph/meta_crash.test.rs",
            text: "use crate::test_scale;\nfn f() { test_scale::cases(4); }",
        },
    ];

    assert_eq!(
        problems(&sources),
        vec![
            "crates/rhizodb/src/graph/meta_crash.test.rs scales its cases with RHIZODB_TEST_SCALE, but `mise run storage-deep` does not select `graph::crash_tests`, so the nightly never runs it at scale"
        ]
    );
}

#[test]
fn reports_a_scaled_module_that_a_skip_drops() {
    let sources = [
        Source {
            path: "crates/rhizodb/src/storage.rs",
            text: "#[cfg(test)]\n#[path = \"storage/bench.test.rs\"]\nmod bench_tests;\n",
        },
        Source {
            path: "crates/rhizodb/src/storage/bench.test.rs",
            text: "crate::test_scale::cases(4)",
        },
    ];
    let skipping = Selection {
        skips: vec!["storage::bench_tests::"],
        ..lib()
    };

    assert_eq!(
        unscaled_problems(&sources, &skipping, &integration()).len(),
        1
    );
}

#[test]
fn reports_a_scaled_test_file_that_no_module_declares() {
    let sources = [Source {
        path: "crates/rhizodb/src/graph/meta.test.rs",
        text: "crate::test_scale::cases(32)",
    }];

    assert_eq!(
        problems(&sources),
        vec![
            "crates/rhizodb/src/graph/meta.test.rs scales its cases with RHIZODB_TEST_SCALE, but no `#[path]` declares it as a module"
        ]
    );
}

#[test]
fn reports_a_scaled_integration_test_the_deep_task_leaves_out() {
    let sources = [
        Source {
            path: "crates/rhizodb/tests/api_misuse.rs",
            text: "mod support;\nuse support::*;\nfn f() { for _ in 0..cases(5) {} }",
        },
        Source {
            path: "crates/rhizodb/tests/own_scale.rs",
            text: "fn f() { std::env::var(\"RHIZODB_TEST_SCALE\"); }",
        },
    ];

    assert_eq!(
        problems(&sources),
        vec![
            "crates/rhizodb/tests/api_misuse.rs scales its cases with RHIZODB_TEST_SCALE, but `mise run random-deep` does not pass `--test api_misuse`, so the nightly never runs it at scale",
            "crates/rhizodb/tests/own_scale.rs scales its cases with RHIZODB_TEST_SCALE, but `mise run random-deep` does not pass `--test own_scale`, so the nightly never runs it at scale",
        ]
    );
}
