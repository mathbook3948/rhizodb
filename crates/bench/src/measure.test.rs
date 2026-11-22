use super::*;
use crate::fingerprint::RowHasher;

fn one_row() -> Fingerprint {
    let mut fingerprint = Fingerprint::default();
    let mut hasher = RowHasher::default();
    hasher.int(1);
    fingerprint.absorb(hasher);
    fingerprint
}

#[test]
fn a_tally_counts_rows_and_keeps_the_first_failure() {
    let mut tally = Tally::default();
    tally.record(Ok(one_row()));
    tally.record(Err(EngineError::Failed("first".to_owned())));
    tally.record(Err(EngineError::Failed("second".to_owned())));

    assert_eq!((tally.ops, tally.rows, tally.errors), (3, 1, 2));
    assert_eq!(tally.failure.as_deref(), Some("first"));
}

#[test]
fn combining_tallies_adds_their_counts() {
    let mut a = Tally::default();
    a.record(Ok(one_row()));
    let mut b = Tally::default();
    b.record(Ok(one_row()));
    b.record(Err(EngineError::Failed("x".to_owned())));
    a.combine(b);

    assert_eq!((a.ops, a.rows, a.errors), (3, 2, 1));
    assert_eq!(a.fingerprint.rows, 2);
}

#[test]
fn timed_returns_the_value_of_the_work() {
    let (elapsed, value) = timed(|| 7);

    assert_eq!(value, 7);
    assert!(elapsed < Duration::from_secs(5));
}
