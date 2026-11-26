mod support;

use rhizodb::Error;
use rhizodb::ErrorKind;
use rhizodb::Parameters;
use support::*;

fn failure(query: &str) -> Error {
    db().execute(query, &Parameters::new()).unwrap_err()
}

#[test]
fn the_offset_points_at_the_token_the_parser_rejected() {
    let query = "MATCH (n RETURN n";
    let error = failure(query);
    assert!(matches!(error.kind(), ErrorKind::Syntax(_)));
    assert_eq!(error.byte_offset(), query.find("RETURN"));
}

#[test]
fn the_offset_counts_bytes_after_multi_byte_text() {
    let query = "RETURN 'é日🙂' ?";
    let error = failure(query);
    assert!(matches!(error.kind(), ErrorKind::Syntax(_)));
    let offset = error.byte_offset().unwrap();
    assert_eq!(offset, query.find('?').unwrap());
    assert_ne!(Some(offset), query.chars().position(|c| c == '?'));
    assert_eq!(&query[offset..], "?");
}

#[test]
fn a_query_that_ends_too_soon_is_reported_at_its_length() {
    let query = "RETURN 'é' +";
    let error = failure(query);
    assert!(matches!(error.kind(), ErrorKind::Syntax(_)));
    assert_eq!(error.byte_offset(), Some(query.len()));
}

#[test]
fn lexer_errors_carry_an_offset_too() {
    let query = "RETURN 'é', 'open";
    let error = failure(query);
    assert!(matches!(error.kind(), ErrorKind::Syntax(_)));
    assert_eq!(error.byte_offset(), query.rfind('\''));
}

#[test]
fn only_a_syntax_error_has_an_offset() {
    for query in [
        "RETURN $missing",
        "RETURN 1 AND 'a'",
        "RETURN x",
        "RETURN 1 / 0",
        "RETURN toUpper(1)",
    ] {
        let error = failure(query);
        assert!(
            !matches!(error.kind(), ErrorKind::Syntax(_)),
            "{query} is a syntax error"
        );
        assert_eq!(error.byte_offset(), None, "{query}");
    }
}

#[test]
fn every_syntax_error_has_an_offset_on_a_character_boundary_within_the_query() {
    for query in [
        "",
        "MATCH (n",
        "MATCH (n RETURN n",
        "RETURN 'é' ?",
        "RETURN 1 \u{2013}",
        "RETURN 99999999999999999999",
        "RETURN 1e999",
        "RETURN 0x",
        r"RETURN '\q'",
        r"RETURN '\uZZZZ'",
        "RETURN 'é",
        "RETURN `é",
        "RETURN /* é",
        "MATCH (a)-[*-1]->(b) RETURN a",
        "RETURN 1 AS x UNION RETURN 2 AS x UNION ALL RETURN 3 AS x",
        &format!("RETURN {}1{}", "(".repeat(5000), ")".repeat(5000)),
    ] {
        let error = failure(query);
        assert!(
            matches!(error.kind(), ErrorKind::Syntax(_)),
            "{query:.40} gave {:?}",
            error.kind()
        );
        let offset = error.byte_offset().expect(query);
        assert!(offset <= query.len(), "{query:.40}: offset {offset}");
        assert!(
            query.is_char_boundary(offset),
            "{query:.40}: offset {offset}"
        );
    }
}
