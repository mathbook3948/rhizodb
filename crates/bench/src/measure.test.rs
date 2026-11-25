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
fn a_tally_counts_rows_and_keeps_the_first_failure_without_counting_it_as_an_operation() {
    let mut tally = Tally::default();
    tally.record(Ok(one_row()));
    tally.record(Err(EngineError::Failed("first".to_owned())));
    tally.record(Err(EngineError::Failed("second".to_owned())));

    assert_eq!((tally.ops, tally.rows, tally.errors), (1, 1, 2));
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

    assert_eq!((a.ops, a.rows, a.errors), (2, 2, 1));
    assert_eq!(a.fingerprint.rows, 2);
}

#[test]
fn timed_returns_the_value_of_the_work() {
    let (elapsed, value) = timed(|| 7);

    assert_eq!(value, 7);
    assert!(elapsed < Duration::from_secs(5));
}

#[test]
fn a_tally_keeps_which_query_gave_which_result() {
    let one = |value: i64| {
        let mut fingerprint = Fingerprint::default();
        let mut hasher = RowHasher::default();
        hasher.int(value);
        fingerprint.absorb(hasher);
        fingerprint
    };
    let mut forward = Tally::default();
    forward.record(Ok(one(1)));
    forward.record(Ok(one(2)));
    let mut swapped = Tally::default();
    swapped.record(Ok(one(2)));
    swapped.record(Ok(one(1)));

    assert_ne!(forward.fingerprint, swapped.fingerprint);
}
