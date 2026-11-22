use super::*;

fn document(text: &str) -> Vec<String> {
    terms(text, Purpose::Document)
}

fn query(text: &str) -> Vec<String> {
    terms(text, Purpose::Query)
}

#[test]
fn words_split_at_unicode_boundaries_and_lowercase() {
    assert_eq!(
        document("Hello, World! Don't stop at 3.14 or e-mail."),
        [
            "hello", "world", "don't", "stop", "at", "3.14", "or", "e", "mail"
        ]
    );
}

#[test]
fn punctuation_and_space_alone_give_no_terms() {
    assert!(document("  ... -- !!").is_empty());
    assert!(document("").is_empty());
}

#[test]
fn combining_marks_stay_inside_their_word() {
    assert_eq!(document("हिन्दी भाषा"), ["हिन्दी", "भाषा"]);
    assert_eq!(document("Ünïcödé café"), ["ünïcödé", "café"]);
}

#[test]
fn a_hangul_word_becomes_pairs_and_single_syllables_in_a_document() {
    assert_eq!(
        document("서울에서 만나요"),
        [
            "서울", "울에", "에서", "서", "울", "에", "서", "만나", "나요", "만", "나", "요"
        ]
    );
}

#[test]
fn a_query_run_looks_for_its_pairs_or_its_one_character() {
    assert_eq!(query("서울에서"), ["서울", "울에", "에서"]);
    assert_eq!(query("집"), ["집"]);
}

#[test]
fn han_and_kana_without_spaces_form_one_run() {
    assert_eq!(
        query("東京都に行く"),
        ["東京", "京都", "都に", "に行", "行く"]
    );
    assert_eq!(query("カタカナ"), ["カタ", "タカ", "カナ"]);
}

#[test]
fn latin_next_to_hangul_splits_into_a_word_and_a_run() {
    assert_eq!(query("RhizoDB에서"), ["rhizodb", "에서"]);
}

#[test]
fn every_query_term_of_a_text_is_a_document_term_of_it() {
    for text in [
        "서울에서 만나요",
        "東京都に行く",
        "Graph DATABASE",
        "a한b글",
    ] {
        let stored = document(text);
        for term in query(text) {
            assert!(stored.contains(&term), "{term} missing from {stored:?}");
        }
    }
}

#[test]
fn control_characters_never_reach_a_term() {
    for text in ["\u{0}\u{903}", "a\u{0}\u{301}b", "x\u{7}\u{200d}y"] {
        let found = document(text);
        assert!(!found.is_empty(), "{text:?} gave no terms");
        for term in found {
            assert!(
                !term.chars().any(char::is_control),
                "{term:?} from {text:?}"
            );
        }
    }
}
