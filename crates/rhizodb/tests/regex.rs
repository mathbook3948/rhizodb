mod support;

use rhizodb::ErrorKind;
use rhizodb::Parameters;
use rhizodb::Value;
use support::*;

#[test]
fn a_regular_expression_must_match_the_whole_string() {
    let db = db();
    assert_eq!(single(&db, "RETURN 'abc' =~ 'a.c'"), boolean(true));
    assert_eq!(single(&db, "RETURN 'abcd' =~ 'a.c'"), boolean(false));
    assert_eq!(single(&db, "RETURN 'xabc' =~ 'abc'"), boolean(false));
    assert_eq!(single(&db, "RETURN 'ab' =~ 'a|ab'"), boolean(true));
    assert_eq!(single(&db, "RETURN '' =~ ''"), boolean(true));
    assert_eq!(single(&db, "RETURN 'a\nb' =~ 'a.b'"), boolean(false));
}

#[test]
fn inline_flags_change_how_the_pattern_matches() {
    let db = db();
    assert_eq!(single(&db, "RETURN 'ABC' =~ '(?i)abc'"), boolean(true));
    assert_eq!(single(&db, "RETURN 'a\nb' =~ '(?s)a.b'"), boolean(true));
    assert_eq!(
        single(&db, "RETURN 'ab' =~ '(?x)a b # a comment'"),
        boolean(true)
    );
    assert_eq!(
        single(&db, "RETURN 'ab' =~ '(?x)a b # comment )'"),
        boolean(true)
    );
}

#[test]
fn a_pattern_matches_unicode_characters() {
    let db = db();
    assert_eq!(single(&db, "RETURN 'café' =~ 'caf.'"), boolean(true));
    assert_eq!(single(&db, "RETURN 'Ünïcödé' =~ '\\\\w+'"), boolean(true));
}

#[test]
fn a_regular_expression_filters_rows() {
    let db = db();
    run(
        &db,
        "CREATE (:P {name: 'Ada'}), (:P {name: 'Alan'}), (:P {name: 'Grace'})",
    );
    let rows = run(
        &db,
        "MATCH (p:P) WHERE p.name =~ 'A.*' RETURN p.name ORDER BY p.name",
    );
    assert_eq!(rows, [vec![string("Ada")], vec![string("Alan")]]);
}

#[test]
fn a_pattern_can_come_from_a_parameter() {
    let db = db();
    let mut parameters = Parameters::new();
    parameters.insert("pattern".to_owned(), Value::String("[0-9]+".to_owned()));
    let result = db
        .execute("RETURN '2026' =~ $pattern AS digits", &parameters)
        .unwrap();
    assert_eq!(result.rows(), [vec![boolean(true)]]);
}

#[test]
fn null_or_a_value_that_is_not_a_string_gives_null() {
    let db = db();
    for query in [
        "RETURN null =~ 'a'",
        "RETURN 'a' =~ null",
        "RETURN 1 =~ 'a'",
        "RETURN 'a' =~ 1",
        "RETURN ['a'] =~ 'a'",
        "RETURN null =~ '('",
        "RETURN 1 =~ '('",
    ] {
        assert_eq!(single(&db, query), Value::Null, "{query}");
    }
}

#[test]
fn an_invalid_pattern_fails_with_its_own_error_kind() {
    let db = db();
    for query in [
        "RETURN 'a' =~ '('",
        "RETURN 'a' =~ 'a)|(b'",
        "RETURN 'aa' =~ '(a)\\\\1'",
        "RETURN 'ab' =~ 'a(?=b)'",
    ] {
        assert_eq!(error_kind(&db, query), ErrorKind::InvalidRegex, "{query}");
    }
}

#[test]
fn an_invalid_pattern_fails_only_when_a_row_reaches_it() {
    let db = db();
    assert!(run(&db, "MATCH (n) WHERE n.name =~ '(' RETURN n").is_empty());
    run(&db, "CREATE ({name: 'a'})");
    assert_eq!(
        error_kind(&db, "MATCH (n) WHERE n.name =~ '(' RETURN n"),
        ErrorKind::InvalidRegex
    );
}

#[test]
fn a_pattern_that_compiles_too_large_fails_as_an_invalid_pattern() {
    let db = db();
    assert_eq!(
        error_kind(&db, "RETURN 'a' =~ '(\\\\w{100}){100}'"),
        ErrorKind::InvalidRegex
    );
}

#[test]
fn a_pattern_that_backtracking_engines_choke_on_matches_quickly() {
    let db = db();
    let text = "a".repeat(10_000);
    let mut parameters = Parameters::new();
    parameters.insert("text".to_owned(), Value::String(format!("{text}!")));
    let result = db
        .execute("RETURN $text =~ '(a+)+$' AS matched", &parameters)
        .unwrap();
    assert_eq!(result.rows(), [vec![boolean(false)]]);
}
