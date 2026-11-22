use super::*;

fn kinds(input: &str) -> Vec<TokenKind> {
    lex(input)
        .unwrap_or_default()
        .into_iter()
        .map(|token| token.kind)
        .collect()
}

fn single(input: &str) -> TokenKind {
    let mut tokens = kinds(input);
    assert_eq!(tokens.len(), 1, "expected one token for {input:?}");
    tokens.remove(0)
}

fn error(input: &str) -> (usize, SyntaxErrorKind) {
    match lex(input) {
        Err(error) => (error.offset, error.kind),
        Ok(tokens) => panic!("expected an error for {input:?}, got {tokens:?}"),
    }
}

fn word(text: &str) -> TokenKind {
    TokenKind::Word {
        text: text.to_owned(),
        quoted: false,
    }
}

#[test]
fn integers_in_decimal_hex_and_octal() {
    assert_eq!(single("42"), TokenKind::Integer(42));
    assert_eq!(single("0"), TokenKind::Integer(0));
    assert_eq!(single("0x1F"), TokenKind::Integer(31));
    assert_eq!(single("0Xff"), TokenKind::Integer(255));
    assert_eq!(single("0o17"), TokenKind::Integer(15));
}

#[test]
fn integer_magnitude_reaches_the_full_unsigned_range() {
    assert_eq!(
        single("9223372036854775808"),
        TokenKind::Integer(9_223_372_036_854_775_808)
    );
    assert_eq!(single("0xFFFFFFFFFFFFFFFF"), TokenKind::Integer(u64::MAX));
}

#[test]
fn integers_beyond_64_bits_overflow() {
    assert_eq!(
        error("18446744073709551616"),
        (0, SyntaxErrorKind::IntegerOverflow)
    );
    assert_eq!(
        error("0x10000000000000000"),
        (0, SyntaxErrorKind::IntegerOverflow)
    );
}

#[test]
fn floats_with_fractions_and_exponents() {
    assert_eq!(single("1.5"), TokenKind::Float(1.5));
    assert_eq!(single(".25"), TokenKind::Float(0.25));
    assert_eq!(single("1e3"), TokenKind::Float(1000.0));
    assert_eq!(single("2.5E-1"), TokenKind::Float(0.25));
    assert_eq!(single("1E+2"), TokenKind::Float(100.0));
}

#[test]
fn range_dots_do_not_turn_integers_into_floats() {
    assert_eq!(
        kinds("1..3"),
        vec![
            TokenKind::Integer(1),
            TokenKind::Punct(Punct::DotDot),
            TokenKind::Integer(3)
        ]
    );
}

#[test]
fn a_malformed_number_is_one_invalid_number_token() {
    for input in ["0x", "0xg", "0o8", "1e", "1e+", "12abc", "1.5x", "1B2c3e67"] {
        assert_eq!(kinds(input), vec![TokenKind::InvalidNumber], "{input}");
    }
}

#[test]
fn a_float_past_the_64_bit_range_overflows() {
    assert_eq!(error("1.34E999"), (0, SyntaxErrorKind::FloatOverflow));
}

#[test]
fn a_character_outside_ascii_that_starts_no_token_is_an_invalid_unicode_character() {
    assert_eq!(
        error("42 \u{2014} 41"),
        (3, SyntaxErrorKind::InvalidUnicodeCharacter)
    );
}

#[test]
fn strings_with_either_quote_and_simple_escapes() {
    assert_eq!(single("'a b'"), TokenKind::String("a b".into()));
    assert_eq!(single("\"a 'b'\""), TokenKind::String("a 'b'".into()));
    assert_eq!(
        single(r#"'\\ \' \" \b \f \n \r \t'"#),
        TokenKind::String("\\ ' \" \u{8} \u{c} \n \r \t".into())
    );
}

#[test]
fn unicode_escapes_decode_to_scalar_values() {
    assert_eq!(single(r"'A'"), TokenKind::String("A".into()));
    assert_eq!(
        single(r"'\U0001F600'"),
        TokenKind::String("\u{1F600}".into())
    );
    assert_eq!(single(r"'😀'"), TokenKind::String("\u{1F600}".into()));
}

#[test]
fn bad_escapes_report_the_backslash_offset() {
    assert_eq!(error(r"'ab\q'"), (3, SyntaxErrorKind::InvalidEscape));
    assert_eq!(error(r"'\u12'"), (1, SyntaxErrorKind::InvalidUnicodeEscape));
    assert_eq!(
        error(r"'\uD83D'"),
        (1, SyntaxErrorKind::InvalidUnicodeEscape)
    );
    assert_eq!(
        error(r"'\uDE00'"),
        (1, SyntaxErrorKind::InvalidUnicodeEscape)
    );
    assert_eq!(
        error(r"'\U00110000'"),
        (1, SyntaxErrorKind::InvalidUnicodeEscape)
    );
}

#[test]
fn unterminated_string_reports_where_it_opened() {
    assert_eq!(error("1 'abc"), (2, SyntaxErrorKind::UnterminatedString));
}

#[test]
fn words_are_unicode_aware_and_backticks_quote_any_text() {
    assert_eq!(single("café_1"), word("café_1"));
    assert_eq!(
        single("`my name`"),
        TokenKind::Word {
            text: "my name".into(),
            quoted: true
        }
    );
    assert_eq!(
        single("`a``b`"),
        TokenKind::Word {
            text: "a`b".into(),
            quoted: true
        }
    );
    assert_eq!(error("`abc"), (0, SyntaxErrorKind::UnterminatedQuotedName));
}

#[test]
fn parameters_take_a_name_or_a_number() {
    assert_eq!(single("$name"), TokenKind::Parameter("name".into()));
    assert_eq!(single("$0"), TokenKind::Parameter("0".into()));
    assert_eq!(error("$ x"), (0, SyntaxErrorKind::UnexpectedCharacter));
}

#[test]
fn multi_character_operators_win_over_their_prefixes() {
    assert_eq!(
        kinds("<> <= >= += =~ .. < > = + ."),
        vec![
            TokenKind::Punct(Punct::Neq),
            TokenKind::Punct(Punct::Le),
            TokenKind::Punct(Punct::Ge),
            TokenKind::Punct(Punct::PlusEq),
            TokenKind::Punct(Punct::RegexMatch),
            TokenKind::Punct(Punct::DotDot),
            TokenKind::Punct(Punct::Lt),
            TokenKind::Punct(Punct::Gt),
            TokenKind::Punct(Punct::Eq),
            TokenKind::Punct(Punct::Plus),
            TokenKind::Punct(Punct::Dot),
        ]
    );
}

#[test]
fn arrows_are_separate_tokens() {
    assert_eq!(
        kinds("<-->"),
        vec![
            TokenKind::Punct(Punct::Lt),
            TokenKind::Punct(Punct::Minus),
            TokenKind::Punct(Punct::Minus),
            TokenKind::Punct(Punct::Gt),
        ]
    );
}

#[test]
fn comments_and_whitespace_are_skipped() {
    assert_eq!(
        kinds("a // line\n /* block\n */ b"),
        vec![word("a"), word("b")]
    );
    assert_eq!(
        error("a /* open"),
        (2, SyntaxErrorKind::UnterminatedComment)
    );
}

#[test]
fn token_spans_are_byte_offsets() {
    let tokens = lex("é 'x'").unwrap_or_default();
    let spans: Vec<(usize, usize)> = tokens.iter().map(|t| (t.start, t.end)).collect();
    assert_eq!(spans, vec![(0, 2), (3, 6)]);
}

#[test]
fn unknown_characters_are_rejected_with_their_offset() {
    assert_eq!(error("a ? b"), (2, SyntaxErrorKind::UnexpectedCharacter));
    assert_eq!(error("a !"), (2, SyntaxErrorKind::UnexpectedCharacter));
}
