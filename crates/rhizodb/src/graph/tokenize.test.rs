use super::*;

fn texts(terms: Vec<Term>) -> Vec<String> {
    terms.into_iter().map(|term| term.text).collect()
}

fn document(text: &str) -> Vec<String> {
    texts(terms(text, Purpose::Document))
}

fn query(text: &str) -> Vec<String> {
    texts(terms(text, Purpose::Query))
}

fn positioned(text: &str, purpose: Purpose) -> Vec<(String, u32)> {
    terms(text, purpose)
        .into_iter()
        .map(|term| (term.text, term.position))
        .collect()
}

fn term(text: &str, position: u32) -> Term {
    Term {
        text: text.to_owned(),
        position,
    }
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
fn thai_lao_khmer_and_myanmar_runs_pair_their_grapheme_clusters() {
    assert_eq!(query("ภาษาไทย"), ["ภา", "าษ", "ษา", "าไ", "ไท", "ทย"]);
    assert_eq!(query("ก่อน"), ["ก\u{e48}อ", "อน"]);
    assert_eq!(query("ພາສາ"), ["ພາ", "າສ", "ສາ"]);
    assert_eq!(query("ភាសា"), ["ភាសា"]);
    assert_eq!(query("မြန်မာ"), ["မြန\u{103a}", "န\u{103a}မ", "မာ"]);
    assert_eq!(document("ไทย"), ["ไท", "ทย", "ไ", "ท", "ย"]);
}

#[test]
fn numbers_and_a_stray_mark_do_not_start_a_run() {
    assert_eq!(query("ปี ๒๕๖๗"), ["ปี", "๒๕๖๗"]);
    assert_eq!(query("a\u{e48}"), ["a\u{e48}"]);
    assert_eq!(document("a\u{e31}"), ["a\u{e31}"]);
    assert_eq!(document("ก\u{e31}a\u{e31}"), ["ก\u{e31}", "a\u{e31}"]);
}

#[test]
fn a_cluster_without_a_base_letter_separates_words() {
    assert_eq!(document("x \u{e31}ก"), ["x", "ก"]);
    assert_eq!(document("\u{e31}ก"), ["ก"]);
    assert_eq!(document("\u{301}abc"), ["abc"]);
    assert_eq!(document("ab\u{7}\u{301}cd"), ["ab", "cd"]);
    for text in ["x \u{e31}ก", " \u{e48}", "a \u{301}b", "\u{e31}"] {
        for term in document(text) {
            assert!(
                !term.chars().any(char::is_whitespace),
                "{term:?} from {text:?}"
            );
        }
    }
}

#[test]
fn a_word_starts_and_ends_at_a_character_that_is_a_letter_or_digit() {
    assert_eq!(document("क.가"), ["क", "가"]);
    assert_eq!(document("a.가"), ["a", "가"]);
    assert_eq!(document("가.a"), ["가", "a"]);
    assert_eq!(document("\"\u{e31}x"), ["x"]);
    assert_eq!(document("\"\u{e31}"), Vec::<String>::new());
    assert_eq!(document("don't 3.14 e.g."), ["don't", "3.14", "e.g"]);
}

#[test]
fn each_hangul_syllable_and_each_loose_jamo_is_a_unit_of_its_own() {
    let khieukh = "\u{110f}";
    assert_eq!(query("ㅋㅋㅋ"), [khieukh.repeat(2), khieukh.repeat(2)]);
    assert_eq!(query("ㅋ"), [khieukh]);
    assert_eq!(query("ㅠㅠ"), ["\u{1172}\u{1172}"]);
    assert_eq!(query("ㄱ가나"), ["\u{1100}가", "가나"]);
    assert_eq!(query("ﾡﾡ"), query("ㄱㄱ"));
    assert_eq!(
        document("ㄱㅅ"),
        ["\u{1100}\u{1109}", "\u{1100}", "\u{1109}"]
    );
}

#[test]
fn a_mark_or_joiner_after_hangul_stays_with_the_unit_before_it() {
    assert_eq!(document("가\u{301}"), ["가\u{301}"]);
    assert_eq!(
        document("한\u{200d}국"),
        ["한\u{200d}국", "한\u{200d}", "국"]
    );
    assert_eq!(
        positioned("한\u{200d}국 x", Purpose::Query),
        [("한\u{200d}국".to_owned(), 0), ("x".to_owned(), 2)]
    );
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
        "ภาษาไทยง่ายมาก",
    ] {
        let stored = document(text);
        for term in query(text) {
            assert!(stored.contains(&term), "{term} missing from {stored:?}");
        }
    }
}

#[test]
fn control_characters_never_reach_a_term() {
    for text in ["x\u{0}\u{903}y", "a\u{0}\u{301}b", "x\u{7}\u{200d}y"] {
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

#[test]
fn composed_and_decomposed_text_give_the_same_terms() {
    assert_eq!(document("cafe\u{301} Åsa"), document("café A\u{30a}sa"));
    assert_eq!(document("cafe\u{301}"), ["café"]);
    assert_eq!(
        document("\u{1109}\u{1165}\u{110b}\u{116e}\u{11af}"),
        document("서울")
    );
}

#[test]
fn the_probe_holds_text_that_normalization_changes() {
    assert!(document(PROBE).contains(&"café".to_owned()));
    assert!(document(PROBE).contains(&"fi".to_owned()));
}

#[test]
fn compatibility_forms_fold_to_their_plain_letters() {
    assert_eq!(
        document("ﬁle ＲｈｉｚｏＤＢ ｶﾀｶﾅ"),
        document("file RhizoDB カタカナ")
    );
}

#[test]
fn a_word_takes_one_position_and_a_run_one_per_cluster() {
    assert_eq!(
        positioned("big 서울에 x", Purpose::Document),
        [
            ("big".to_owned(), 0),
            ("서울".to_owned(), 1),
            ("울에".to_owned(), 2),
            ("서".to_owned(), 1),
            ("울".to_owned(), 2),
            ("에".to_owned(), 3),
            ("x".to_owned(), 4),
        ]
    );
    assert_eq!(
        positioned("집 서울 x", Purpose::Query),
        [
            ("집".to_owned(), 0),
            ("서울".to_owned(), 1),
            ("x".to_owned(), 3),
        ]
    );
}

#[test]
fn quoted_text_is_a_phrase_and_other_terms_stand_alone() {
    assert_eq!(
        clauses("graph \"Store  of\" data graph"),
        [
            Clause::Term("data".to_owned()),
            Clause::Term("graph".to_owned()),
            Clause::Phrase(vec![term("store", 0), term("of", 1)]),
        ]
    );
}

#[test]
fn a_phrase_of_one_term_is_the_term_and_an_unclosed_quote_runs_to_the_end() {
    assert_eq!(
        clauses("\"graph\" \"\" \"!!\" \"open phrase"),
        [
            Clause::Term("graph".to_owned()),
            Clause::Phrase(vec![term("open", 0), term("phrase", 1)]),
        ]
    );
}

#[test]
fn a_phrase_keeps_the_positions_of_a_run() {
    assert_eq!(
        clauses("\"서울에서 만나\""),
        [Clause::Phrase(vec![
            term("서울", 0),
            term("울에", 1),
            term("에서", 2),
            term("만나", 4),
        ])]
    );
}

#[test]
fn a_star_right_after_a_word_makes_it_a_prefix() {
    assert_eq!(
        clauses("graph* data stor*e graph * Ｒｈｉｚｏ＊ \"in* phrase\" 東京*"),
        [
            Clause::Term("data".to_owned()),
            Clause::Term("e".to_owned()),
            Clause::Term("graph".to_owned()),
            Clause::Term("東京".to_owned()),
            Clause::Prefix("graph".to_owned()),
            Clause::Prefix("rhizo".to_owned()),
            Clause::Prefix("stor".to_owned()),
            Clause::Phrase(vec![term("in", 0), term("phrase", 1)]),
        ]
    );
    assert!(clauses("* **").is_empty());
}
