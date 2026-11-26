//! The committed index definitions of `Graph` in `crates/rhizodb/src/graph.rs`. A reader takes
//! the catalog lock and begins its storage snapshot while holding it. A commit that changes an
//! index holds the catalog lock across the storage commit and then publishes the new
//! definitions. A writer reads the catalog after it has taken the storage write lock. Every
//! transaction must see the definitions its own snapshot holds.

use shuttle::sync::Arc;
use shuttle::sync::Condvar;
use shuttle::sync::Mutex;
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

#[derive(Default)]
struct Graph {
    storage: Storage,
    catalog: Mutex<u64>,
}

impl Graph {
    fn read(&self) {
        let catalog = self.catalog.lock().unwrap();
        let snapshot = self.storage.begin_read();
        assert_eq!(
            *catalog, snapshot,
            "a reader got definitions of another snapshot"
        );
    }

    fn write(&self, changes_an_index: bool) {
        let snapshot = self.storage.begin_write();
        let seen = *self.catalog.lock().unwrap();
        assert_eq!(
            seen, snapshot,
            "a writer got definitions of another snapshot"
        );
        thread::yield_now();
        if changes_an_index {
            let mut catalog = self.catalog.lock().unwrap();
            self.storage.commit(snapshot + 1);
            *catalog = snapshot + 1;
        } else {
            self.storage.commit(snapshot);
        }
    }
}

fn readers_and_writers() {
    let graph = Arc::new(Graph::default());
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
fn every_transaction_sees_the_index_definitions_of_its_snapshot() {
    shuttle::check_random(readers_and_writers, schedules());
    shuttle::check_pct(readers_and_writers, schedules(), 3);
}

/// 10000 schedules, times the scale that `RHIZODB_TEST_SCALE` sets for nightly runs.
fn schedules() -> usize {
    let scale = std::env::var("RHIZODB_TEST_SCALE")
        .ok()
        .and_then(|scale| scale.parse::<usize>().ok())
        .unwrap_or(1);
    10_000 * scale.max(1)
}
