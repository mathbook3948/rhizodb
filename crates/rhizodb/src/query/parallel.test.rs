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
fn a_waiting_read_query_starts_once_a_core_beside_the_running_ones_is_spare() {
    let workers = Workers::with_cores(3);
    let running: Vec<Busy<'_>> = (0..3).map(|_| workers.enter()).collect();
    let started = AtomicBool::new(false);

    thread::scope(|scope| {
        scope.spawn(|| {
            let _turn = workers.enter();
            started.store(true, Ordering::SeqCst);
        });
        while workers.queued.load(Ordering::SeqCst) == 0 {
            thread::yield_now();
        }
        let mut running = running;
        running.pop();
        thread::sleep(std::time::Duration::from_millis(50));
        assert!(
            !started.load(Ordering::SeqCst),
            "a queued query took the spare core"
        );
        running.pop();
    });

    assert!(started.load(Ordering::SeqCst));
}

#[test]
fn while_writes_flow_a_read_query_leaves_a_core_to_the_writer() {
    let workers = Workers::with_cores(2);
    let first = workers.enter();
    workers.writing();
    let started = AtomicBool::new(false);

    thread::scope(|scope| {
        scope.spawn(|| {
            let _second = workers.enter();
            started.store(true, Ordering::SeqCst);
        });
        thread::sleep(std::time::Duration::from_millis(50));
        assert!(
            !started.load(Ordering::SeqCst),
            "a reader took the writer's core"
        );
        drop(first);
    });

    assert!(started.load(Ordering::SeqCst));
}

#[test]
fn waiting_read_queries_start_in_the_order_they_came() {
    let workers = Workers::with_cores(1);
    let running = workers.enter();
    let started = Mutex::new(Vec::new());

    thread::scope(|scope| {
        for query in 0..4 {
            let (workers, started) = (&workers, &started);
            scope.spawn(move || {
                let _turn = workers.enter();
                started.lock().unwrap().push(query);
            });
            while workers.queued.load(Ordering::SeqCst) < query + 1 {
                thread::yield_now();
            }
        }
        drop(running);
    });

    assert_eq!(*started.lock().unwrap(), [0, 1, 2, 3]);
}
