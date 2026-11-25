use super::*;

#[test]
fn a_lone_query_takes_every_other_thread_as_a_helper() {
    let workers = Workers::with_cores(4);
    let _running = workers.enter();

    let helpers = workers.reserve(3, 4);

    assert_eq!(helpers.threads, 3);
}

#[test]
fn queries_that_keep_every_thread_busy_leave_no_helpers() {
    let workers = Workers::with_cores(4);
    let running: Vec<Busy<'_>> = (0..4).map(|_| workers.enter()).collect();

    assert_eq!(workers.reserve(3, 4).threads, 0);
    drop(running);
    let _running = workers.enter();
    assert_eq!(workers.reserve(3, 4).threads, 3);
}

#[test]
fn helpers_count_as_busy_until_their_split_ends() {
    let workers = Workers::with_cores(4);
    let _first = workers.enter();
    let _second = workers.enter();

    let helpers = workers.reserve(3, 4);
    assert_eq!(helpers.threads, 2);
    assert_eq!(workers.reserve(3, 4).threads, 0);
    drop(helpers);
    assert_eq!(workers.reserve(3, 4).threads, 2);
}

#[test]
fn a_read_query_waits_while_every_core_is_busy_and_starts_once_one_frees() {
    let workers = Workers::with_cores(2);
    workers.writing();
    let first = workers.enter();
    let second = workers.enter();
    let started = AtomicBool::new(false);

    thread::scope(|scope| {
        scope.spawn(|| {
            let _third = workers.enter();
            started.store(true, Ordering::SeqCst);
        });
        thread::sleep(std::time::Duration::from_millis(50));
        assert!(
            !started.load(Ordering::SeqCst),
            "a third query ran on two cores"
        );
        drop(first);
    });

    assert!(started.load(Ordering::SeqCst));
    drop(second);
    assert_eq!(workers.reserve(2, 2).threads, 2);
}

#[test]
fn read_queries_take_no_turns_while_no_write_flows() {
    let workers = Workers::with_cores(1);

    let running: Vec<Busy<'_>> = (0..3).map(|_| workers.enter()).collect();

    assert_eq!(running.len(), 3);
    assert_eq!(workers.reserve(1, 4).threads, 1);
}
