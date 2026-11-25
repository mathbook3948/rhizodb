use super::*;

#[test]
fn a_lone_query_takes_every_other_thread_as_a_helper() {
    let workers = Workers::default();
    let _running = workers.enter();

    let helpers = workers.reserve(3, 4);

    assert_eq!(helpers.threads, 3);
}

#[test]
fn queries_that_keep_every_thread_busy_leave_no_helpers() {
    let workers = Workers::default();
    let running: Vec<Busy<'_>> = (0..4).map(|_| workers.enter()).collect();

    assert_eq!(workers.reserve(3, 4).threads, 0);
    drop(running);
    let _running = workers.enter();
    assert_eq!(workers.reserve(3, 4).threads, 3);
}

#[test]
fn helpers_count_as_busy_until_their_split_ends() {
    let workers = Workers::default();
    let _first = workers.enter();
    let _second = workers.enter();

    let helpers = workers.reserve(3, 4);
    assert_eq!(helpers.threads, 2);
    assert_eq!(workers.reserve(3, 4).threads, 0);
    drop(helpers);
    assert_eq!(workers.reserve(3, 4).threads, 2);
}
