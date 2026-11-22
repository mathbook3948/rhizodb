use super::*;

fn refusal(arguments: &[&str]) -> Option<&'static str> {
    let args = Arguments::from_iter(std::iter::once("tck").chain(arguments.iter().copied()));
    bless_refusal(&args)
}

#[test]
fn a_full_default_run_may_be_blessed() {
    assert_eq!(refusal(&[]), None);
    assert_eq!(refusal(&["--test-threads", "2", "--quiet"]), None);
}

#[test]
fn a_run_of_part_of_the_suite_may_not_be_blessed() {
    for arguments in [
        &["Match1"][..],
        &["--skip", "Match1"],
        &["--ignored"],
        &["--include-ignored"],
        &["--exact", "Match1"],
        &["--list"],
    ] {
        assert!(refusal(arguments).is_some(), "{arguments:?}");
    }
}
