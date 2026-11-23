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

fn scratch_root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("xtask-hooks-{name}-{}", std::process::id()));
    fs::create_dir_all(root.join(HOOKS_DIR)).unwrap();
    root
}

#[test]
fn accepts_the_relative_hooks_dir() {
    assert!(installs_hooks(".githooks", Path::new("/nowhere"), None));
}

#[test]
fn accepts_an_absolute_path_to_the_workspace_hooks_dir() {
    let root = scratch_root("workspace");
    let value = root.join(HOOKS_DIR);

    assert!(installs_hooks(value.to_str().unwrap(), &root, None));
}

#[test]
fn accepts_an_absolute_path_to_the_main_worktree_hooks_dir() {
    let main = scratch_root("main");
    let linked = scratch_root("linked");
    let value = main.join(HOOKS_DIR);

    assert!(installs_hooks(
        value.to_str().unwrap(),
        &linked,
        Some(&main)
    ));
}

#[test]
fn rejects_a_path_to_another_hooks_dir() {
    let root = scratch_root("own");
    let other = scratch_root("other");
    let value = other.join(HOOKS_DIR);

    assert!(!installs_hooks(value.to_str().unwrap(), &root, None));
}

#[test]
fn rejects_an_unset_or_relative_other_value() {
    let root = scratch_root("unset");

    assert!(!installs_hooks("", &root, None));
    assert!(!installs_hooks("hooks", &root, None));
    assert!(!installs_hooks("/dev/null", &root, None));
}
