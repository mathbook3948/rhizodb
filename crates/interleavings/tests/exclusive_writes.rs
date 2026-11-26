//! The exclusive writes of `Graph::exclusive_writes` in `crates/rhizodb/src/graph.rs`, which an
//! import holds across the write transactions of its batches. `Graph::write` on another thread
//! waits while the slot names another thread's exclusive writes, takes the storage write lock,
//! and checks the slot again, giving the lock back and waiting when exclusive writes began in
//! between. No write of another thread may commit between two batches.

use shuttle::sync::Arc;
use shuttle::sync::Condvar;
use shuttle::sync::Mutex;
use shuttle::thread;
use shuttle::thread::ThreadId;

/// The storage write lock: `begin_write` waits while a write transaction is live.
#[derive(Default)]
struct StorageLock {
    live: Mutex<bool>,
    released: Condvar,
}

impl StorageLock {
    fn begin_write(&self) {
        let mut live = self.live.lock().unwrap();
        while *live {
            live = self.released.wait(live).unwrap();
        }
        *live = true;
    }

    fn end_write(&self) {
        *self.live.lock().unwrap() = false;
        self.released.notify_all();
    }
}

#[derive(Clone, Copy)]
enum Check {
    /// Check the slot only before taking the storage write lock.
    Before,
    /// What `Graph::write` does: check it again after taking the lock.
    BeforeAndAfter,
}

struct Graph {
    storage: StorageLock,
    exclusive: Mutex<Option<ThreadId>>,
    released: Condvar,
    check: Check,
    commits: Mutex<Vec<&'static str>>,
}

impl Graph {
    fn new(check: Check) -> Self {
        Self {
            storage: StorageLock::default(),
            exclusive: Mutex::new(None),
            released: Condvar::new(),
            check,
            commits: Mutex::new(Vec::new()),
        }
    }

    fn excluded(&self, current: ThreadId) -> bool {
        self.exclusive
            .lock()
            .unwrap()
            .is_some_and(|owner| owner != current)
    }

    fn wait_for_exclusive(&self, current: ThreadId) {
        let mut exclusive = self.exclusive.lock().unwrap();
        while exclusive.is_some_and(|owner| owner != current) {
            exclusive = self.released.wait(exclusive).unwrap();
        }
    }

    fn commit(&self, name: &'static str) {
        let current = thread::current().id();
        loop {
            self.wait_for_exclusive(current);
            self.storage.begin_write();
            match self.check {
                Check::BeforeAndAfter if self.excluded(current) => self.storage.end_write(),
                _ => break,
            }
        }
        thread::yield_now();
        self.commits.lock().unwrap().push(name);
        self.storage.end_write();
    }

    fn import(&self) {
        let current = thread::current().id();
        {
            let mut exclusive = self.exclusive.lock().unwrap();
            while exclusive.is_some() {
                exclusive = self.released.wait(exclusive).unwrap();
            }
            *exclusive = Some(current);
        }
        self.commit("first batch");
        thread::yield_now();
        self.commit("second batch");
        *self.exclusive.lock().unwrap() = None;
        self.released.notify_all();
    }
}

fn an_import_and_a_writer(check: Check) {
    let graph = Arc::new(Graph::new(check));
    let importer = {
        let graph = Arc::clone(&graph);
        thread::spawn(move || graph.import())
    };
    let writer = {
        let graph = Arc::clone(&graph);
        thread::spawn(move || graph.commit("other"))
    };
    importer.join().unwrap();
    writer.join().unwrap();
    let commits = graph.commits.lock().unwrap();
    let first = commits.iter().position(|name| *name == "first batch");
    let second = commits.iter().position(|name| *name == "second batch");
    assert_eq!(
        second,
        first.map(|first| first + 1),
        "a write landed between two batches: {commits:?}"
    );
}

#[test]
#[should_panic]
fn checking_only_before_the_storage_lock_lets_a_write_in_between_batches() {
    shuttle::check_random(|| an_import_and_a_writer(Check::Before), schedules());
}

#[test]
fn checking_again_after_the_storage_lock_keeps_other_writes_out_of_an_import() {
    shuttle::check_random(
        || an_import_and_a_writer(Check::BeforeAndAfter),
        schedules(),
    );
    shuttle::check_pct(
        || an_import_and_a_writer(Check::BeforeAndAfter),
        schedules(),
        3,
    );
}

/// 10000 schedules, times the scale that `RHIZODB_TEST_SCALE` sets for nightly runs.
fn schedules() -> usize {
    let scale = std::env::var("RHIZODB_TEST_SCALE")
        .ok()
        .and_then(|scale| scale.parse::<usize>().ok())
        .unwrap_or(1);
    10_000 * scale.max(1)
}
