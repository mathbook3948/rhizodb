use super::*;

const CONFIG: &str = "ruleDirs:\n  - guards\nlanguageGlobs:\n  bash:\n    - pre-commit\n    - \"commit-msg\"\n  yaml:\n    - pre-push\n";

#[test]
fn reads_the_bash_globs_only() {
    assert_eq!(bash_globs(CONFIG), vec!["pre-commit", "commit-msg"]);
}

#[test]
fn reports_a_hook_that_ast_grep_would_not_scan() {
    let hooks = vec!["commit-msg".to_string(), "pre-push".to_string()];

    assert_eq!(unscanned_hooks(&hooks, CONFIG), vec!["pre-push"]);
}

#[test]
fn reports_every_hook_when_no_bash_globs_exist() {
    let hooks = vec!["pre-commit".to_string()];

    assert_eq!(
        unscanned_hooks(&hooks, "ruleDirs:\n  - guards\n"),
        vec!["pre-commit"]
    );
}
