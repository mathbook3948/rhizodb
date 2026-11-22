use super::*;

fn differ() -> Outcome {
    Outcome::Differ("rows".into())
}

fn rejects() -> Outcome {
    Outcome::LadybugRejects("Parser exception: no".into())
}

#[test]
fn parse_and_render_round_trip_sorted() {
    let text = "differ\tq2\tnulls sort first\nladybug-rejects\tq1\tParser exception\n";
    let expectations = Expectations::parse(text).expect("valid");

    assert_eq!(
        expectations.render(),
        "ladybug-rejects\tq1\tParser exception\ndiffer\tq2\tnulls sort first\n"
    );
}

#[test]
fn parse_rejects_malformed_lines() {
    for (text, message) in [
        ("differ\tq1", "tabs"),
        ("agree\tq1\tx", "unknown kind"),
        ("differ\tq1\ta\ndiffer\tq1\tb", "twice"),
    ] {
        let error = Expectations::parse(text).expect_err(text);
        assert!(error.message.contains(message), "{error}");
    }
}

#[test]
fn an_unlisted_query_must_agree() {
    let expectations = Expectations::default();

    assert_eq!(expectations.judge("q", &Outcome::Agree), Ok(()));
    assert!(
        expectations
            .judge("q", &differ())
            .expect_err("unlisted")
            .contains("DIFFERENTIAL_BLESS=1")
    );
}

#[test]
fn a_listed_query_must_keep_its_kind() {
    let expectations = Expectations::parse("differ\tq\tnulls sort first\n").expect("valid");

    assert_eq!(expectations.judge("q", &differ()), Ok(()));
    assert!(expectations.judge("q", &Outcome::Agree).is_err());
    assert!(expectations.judge("q", &rejects()).is_err());
}

#[test]
fn a_difference_needs_a_reason() {
    let expectations = Expectations::parse(&format!("differ\tq\t{UNEXPLAINED}\n")).expect("valid");

    assert!(
        expectations
            .judge("q", &differ())
            .expect_err("unexplained")
            .contains("does not say why")
    );
}

#[test]
fn bless_keeps_reasons_and_marks_new_differences() {
    let old =
        Expectations::parse("differ\tkept\tnulls sort first\ndiffer\tgone\tx\n").expect("valid");
    let outcomes = [
        ("kept", differ()),
        ("new", differ()),
        ("gone", Outcome::Agree),
        ("rejected", rejects()),
    ];

    let blessed = old.bless(outcomes.iter().map(|(name, outcome)| (*name, outcome)));

    assert_eq!(
        blessed.render(),
        format!(
            "differ\tkept\tnulls sort first\ndiffer\tnew\t{UNEXPLAINED}\nladybug-rejects\trejected\tParser exception: no\n"
        )
    );
}

#[test]
fn stale_names_are_reported() {
    let expectations = Expectations::parse("differ\tq\tx\n").expect("valid");

    assert_eq!(expectations.stale(&BTreeSet::new()), vec!["q"]);
    assert!(expectations.stale(&BTreeSet::from(["q"])).is_empty());
}
