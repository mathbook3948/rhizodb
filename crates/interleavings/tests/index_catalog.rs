//! The committed index definitions of `Graph` in `crates/rhizodb/src/graph.rs`. A commit that
//! changes an index makes a count odd, commits its storage, publishes the new definitions, and
//! makes the count even again, holding across the storage commit only a lock that such commits
//! share, since the storage lets the next writer in before the definitions are published. A reader or a writer
//! uses the published definitions only when the count was even, for a reader both before and
//! after it began its storage snapshot, and otherwise reads them from its snapshot. Every
//! transaction must see the definitions its own snapshot holds.

use shuttle::sync::Arc;
use shuttle::sync::Condvar;
use shuttle::sync::Mutex;
use shuttle::sync::atomic::AtomicU64;
use shuttle::sync::atomic::Ordering;
use shuttle::thread;

/// The definitions version each committed storage version holds.
#[derive(Default)]
struct Storage {
    committed: Mutex<u64>,
    live_writer: Mutex<bool>,
    released: Condvar,
}

impl Storage {
    fn begin_read(&self) -> u64 {
        *self.committed.lock().unwrap()
    }

    fn begin_write(&self) -> u64 {
        let mut live = self.live_writer.lock().unwrap();
        while *live {
            live = self.released.wait(live).unwrap();
        }
        *live = true;
        *self.committed.lock().unwrap()
    }

    fn commit(&self, definitions: u64) {
        *self.committed.lock().unwrap() = definitions;
        *self.live_writer.lock().unwrap() = false;
        self.released.notify_all();
    }
}

#[derive(Clone, Copy)]
enum Check {
    /// Check the count only before beginning the snapshot.
    Before,
    /// What `Graph::read` does: check it before and after.
    BeforeAndAfter,
}

struct Graph {
    storage: Storage,
    catalog: Mutex<u64>,
    changes: AtomicU64,
    publishing: Mutex<()>,
    check: Check,
}

impl Graph {
    fn new(check: Check) -> Self {
        Self {
            storage: Storage::default(),
            catalog: Mutex::new(0),
            changes: AtomicU64::new(0),
            publishing: Mutex::new(()),
            check,
        }
    }

    fn read(&self) {
        let before = self.changes.load(Ordering::SeqCst);
        let snapshot = self.storage.begin_read();
        let published = *self.catalog.lock().unwrap();
        let unchanged = match self.check {
            Check::Before => before.is_multiple_of(2),
            Check::BeforeAndAfter => {
                before.is_multiple_of(2) && self.changes.load(Ordering::SeqCst) == before
            }
        };
        if unchanged {
            assert_eq!(
                published, snapshot,
                "a reader got definitions of another snapshot"
            );
        }
    }

    fn write(&self, changes_an_index: bool) {
        let snapshot = self.storage.begin_write();
        if self.changes.load(Ordering::SeqCst).is_multiple_of(2) {
            let published = *self.catalog.lock().unwrap();
            assert_eq!(
                published, snapshot,
                "a writer got definitions of another snapshot"
            );
        }
        thread::yield_now();
        if changes_an_index {
            let _publishing = self.publishing.lock().unwrap();
            self.changes.fetch_add(1, Ordering::SeqCst);
            self.storage.commit(snapshot + 1);
            thread::yield_now();
            *self.catalog.lock().unwrap() = snapshot + 1;
            self.changes.fetch_add(1, Ordering::SeqCst);
        } else {
            self.storage.commit(snapshot);
        }
    }
}

fn readers_and_writers(check: Check) {
    let graph = Arc::new(Graph::new(check));
    let mut threads = Vec::new();
    for changes in [true, false] {
        let graph = Arc::clone(&graph);
        threads.push(thread::spawn(move || {
            graph.write(changes);
            graph.write(!changes);
        }));
    }
    for _ in 0..2 {
        let graph = Arc::clone(&graph);
        threads.push(thread::spawn(move || {
            graph.read();
            graph.read();
        }));
    }
    for thread in threads {
        thread.join().unwrap();
    }
    graph.read();
}

#[test]
#[ignore = "left out of mise run check, run by mise run random"]
#[should_panic]
fn a_reader_that_checks_only_before_its_snapshot_can_get_stale_definitions() {
    shuttle::check_random(|| readers_and_writers(Check::Before), schedules());
}

#[test]
#[ignore = "left out of mise run check, run by mise run random"]
fn every_transaction_sees_the_index_definitions_of_its_snapshot() {
    shuttle::check_random(|| readers_and_writers(Check::BeforeAndAfter), schedules());
    shuttle::check_pct(
        || readers_and_writers(Check::BeforeAndAfter),
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
