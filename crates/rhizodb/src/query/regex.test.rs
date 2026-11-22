use super::*;

#[test]
fn matching_stays_correct_when_a_query_reads_more_patterns_than_it_keeps() {
    let patterns = Patterns::default();
    for round in 0..3 {
        for length in 0..(CACHED_PATTERNS * 2) {
            let pattern = format!("a{{{length}}}");
            let text = "a".repeat(length);
            assert!(
                patterns.matches(&text, &pattern).unwrap(),
                "{round} {pattern}"
            );
            assert!(!patterns.matches(&format!("{text}a"), &pattern).unwrap());
        }
    }
}

#[test]
fn a_failed_compile_is_not_kept() {
    let patterns = Patterns::default();
    for _ in 0..2 {
        assert_eq!(
            patterns.matches("a", "(").unwrap_err().kind(),
            ErrorKind::InvalidRegex
        );
    }
}
