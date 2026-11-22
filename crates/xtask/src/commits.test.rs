use super::*;

#[test]
fn accepts_conventional_subjects() {
    for subject in [
        "fix: drop a stale key",
        "fix(index): drop a stale key",
        "refactor(query-plan)!: rename the planner",
        "docs(changelog): record a fix",
    ] {
        assert!(parse_subject(subject).is_some(), "{subject}");
    }
}

#[test]
fn rejects_subjects_outside_the_convention() {
    for subject in [
        "Fix a stale key",
        "feature: add a thing",
        "fix(Index): drop a stale key",
        "fix(): drop a stale key",
        "fix:  drop a stale key",
        "fix: ",
        "fix(index) drop a stale key",
    ] {
        assert!(parse_subject(subject).is_none(), "{subject}");
    }
}

#[test]
fn reads_the_type_and_the_breaking_mark() {
    assert_eq!(
        parse_subject("feat(index)!: drop the old format"),
        Some(Subject {
            kind: "feat",
            breaking: true
        })
    );
}

#[test]
fn requires_the_changelog_with_a_feat_commit() {
    assert_eq!(
        commit_problems(
            "feat(index): search relationships",
            &["crates/rhizodb/src/lib.rs"]
        )
        .len(),
        1
    );
    assert!(
        commit_problems(
            "feat(index): search relationships",
            &["crates/rhizodb/src/lib.rs", "CHANGELOG.md"]
        )
        .is_empty()
    );
}

#[test]
fn requires_the_changelog_with_a_breaking_commit_of_any_type() {
    assert_eq!(
        commit_problems("refactor!: rename Graph", &["src/lib.rs"]).len(),
        1
    );
}

#[test]
fn lets_other_types_leave_the_changelog_alone() {
    assert!(commit_problems("fix(query): keep a taint", &["src/bind.rs"]).is_empty());
}

#[test]
fn skips_merges_fixups_and_squashes() {
    for subject in ["Merge branch 'main'", "fixup! feat: a", "squash! feat: a"] {
        assert!(commit_problems(subject, &[]).is_empty(), "{subject}");
    }
}

#[test]
fn reports_a_subject_outside_the_convention() {
    assert_eq!(commit_problems("Add a thing", &["CHANGELOG.md"]).len(), 1);
}
