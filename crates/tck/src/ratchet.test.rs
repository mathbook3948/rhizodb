use super::*;

fn failed() -> Verdict {
    Verdict::Failed("wrong rows".into())
}

fn listed() -> Ratchet {
    Ratchet::parse("a.feature: [1] one\n")
}

#[test]
fn a_listed_scenario_that_passes_is_fine() {
    assert_eq!(
        listed().judge("a.feature: [1] one", &Verdict::Passed),
        Ok(())
    );
}

#[test]
fn a_listed_scenario_that_fails_is_a_regression() {
    let error = listed()
        .judge("a.feature: [1] one", &failed())
        .expect_err("regression");

    assert!(error.contains("regression"), "{error}");
}

#[test]
fn an_unlisted_scenario_that_passes_asks_for_a_bless() {
    let error = listed()
        .judge("a.feature: [2] two", &Verdict::Passed)
        .expect_err("unrecorded pass");

    assert!(error.contains("TCK_BLESS=1"), "{error}");
}

#[test]
fn an_unlisted_scenario_that_fails_is_fine() {
    assert_eq!(listed().judge("a.feature: [2] two", &failed()), Ok(()));
}

#[test]
fn a_harness_error_always_fails() {
    let verdict = Verdict::HarnessError("unrecognized step".into());

    assert!(listed().judge("a.feature: [1] one", &verdict).is_err());
    assert!(listed().judge("a.feature: [2] two", &verdict).is_err());
}

#[test]
fn names_that_no_longer_exist_are_stale() {
    let ratchet = Ratchet::parse("a.feature: [1] one\nb.feature: [1] gone\n");
    let existing: BTreeSet<&str> = ["a.feature: [1] one"].into();

    assert_eq!(ratchet.stale(&existing), vec!["b.feature: [1] gone"]);
    assert!(listed().stale(&existing).is_empty());
}

#[test]
fn renders_names_sorted_one_per_line_and_reads_them_back() {
    let ratchet = Ratchet::from_passing(["b".to_string(), "a".to_string()]);

    assert_eq!(ratchet.render(), "a\nb\n");
    assert_eq!(Ratchet::parse(&ratchet.render()), ratchet);
}

#[test]
fn an_empty_file_is_an_empty_ratchet() {
    assert!(Ratchet::parse("").is_empty());
    assert_eq!(Ratchet::default().render(), "");
}
