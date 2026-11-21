use super::*;

#[test]
fn reports_line_and_name_of_each_forbidden_char() {
    let text = "plain - hyphen\na \u{2014} b\n\u{300C}quoted\u{300D}";

    assert_eq!(
        forbidden_chars_in(text),
        vec![
            ForbiddenChar {
                line: 2,
                name: "em dash"
            },
            ForbiddenChar {
                line: 3,
                name: "left corner bracket"
            },
            ForbiddenChar {
                line: 3,
                name: "right corner bracket"
            },
        ]
    );
}

#[test]
fn allows_ascii_punctuation() {
    assert!(forbidden_chars_in("a - b <T> . * | ~").is_empty());
}

#[test]
fn skips_vendored_skills_and_tck_files() {
    assert!(is_vendored(".claude/skills/rigor-mode/SKILL.md"));
    assert!(is_vendored(
        "crates/tck/vendor/opencypher/features/Match1.feature"
    ));
}

#[test]
fn checks_project_files_next_to_vendored_ones() {
    assert!(!is_vendored("crates/tck/src/lib.rs"));
    assert!(!is_vendored("docs/README.md"));
}
