//! The writer slot of `Graph::write` in `crates/rhizodb/src/graph.rs`: a thread checks the slot,
//! takes redb's write lock, and records itself in the slot, and the write transaction gives the
//! lock back before the slot is cleared, because `GraphTx` drops its storage transaction before
//! its `WriterClaim`.

use shuttle::sync::Arc;
use shuttle::sync::Condvar;
use shuttle::sync::Mutex;
use shuttle::thread;
use shuttle::thread::ThreadId;

/// redb's write lock: `begin_write` waits while a write transaction is live.
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
enum Clear {
    /// What `WriterClaim::drop` did before the fix: always empty the slot.
    Always,
    /// What `WriterClaim::drop` does: empty the slot only while it still names its own thread.
    IfOwn,
}

struct Graph {
    storage: StorageLock,
    writer: Mutex<Option<ThreadId>>,
    clear: Clear,
}

struct WriteTx<'g> {
    graph: &'g Graph,
    owner: ThreadId,
}

#[derive(Debug, PartialEq)]
struct WriterOnThisThread;

impl Graph {
    fn new(clear: Clear) -> Self {
        Self {
            storage: StorageLock::default(),
            writer: Mutex::new(None),
            clear,
        }
    }

    fn write(&self) -> Result<WriteTx<'_>, WriterOnThisThread> {
        let current = thread::current().id();
        if *self.writer.lock().unwrap() == Some(current) {
            return Err(WriterOnThisThread);
        }
        self.storage.begin_write();
        *self.writer.lock().unwrap() = Some(current);
        Ok(WriteTx {
            graph: self,
            owner: current,
        })
    }
}

impl Drop for WriteTx<'_> {
    fn drop(&mut self) {
        self.graph.storage.end_write();
        let mut writer = self.graph.writer.lock().unwrap();
        match self.graph.clear {
            Clear::Always => *writer = None,
            Clear::IfOwn => {
                if *writer == Some(self.owner) {
                    *writer = None;
                }
            }
        }
    }
}

/// Two threads each open a write transaction twice. While a thread holds one, beginning another
/// on it must fail, since waiting would wait for itself forever.
fn two_writers(clear: Clear) {
    let graph = Arc::new(Graph::new(clear));
    let threads: Vec<_> = (0..2)
        .map(|_| {
            let graph = Arc::clone(&graph);
            thread::spawn(move || {
                for _ in 0..2 {
                    let tx = graph.write().unwrap();
                    thread::yield_now();
                    assert_eq!(
                        graph.write().err(),
                        Some(WriterOnThisThread),
                        "the slot lost the thread that holds the write transaction"
                    );
                    drop(tx);
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
}

#[test]
#[should_panic]
fn a_claim_that_always_empties_the_slot_loses_the_next_holder() {
    shuttle::check_random(|| two_writers(Clear::Always), schedules());
}

#[test]
fn clearing_only_an_own_claim_keeps_the_holder_in_the_slot() {
    shuttle::check_random(|| two_writers(Clear::IfOwn), schedules());
    shuttle::check_pct(|| two_writers(Clear::IfOwn), schedules(), 3);
}

/// 10000 schedules, times the scale that `RHIZODB_TEST_SCALE` sets for nightly runs.
fn schedules() -> usize {
    let scale = std::env::var("RHIZODB_TEST_SCALE")
        .ok()
        .and_then(|scale| scale.parse::<usize>().ok())
        .unwrap_or(1);
    10_000 * scale.max(1)
}
