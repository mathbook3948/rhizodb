//! How a checkpoint clears the slot of a dead process in `Session::oldest_other_generation` in
//! `crates/rhizodb/src/engine/session.rs`, while a joining process claims that slot: the joiner
//! takes the slot's liveness lock and then stores its generation, and the checkpoint finds the
//! owner dead by getting that lock itself.

use shuttle::sync::Arc;
use shuttle::sync::Mutex;
use shuttle::sync::atomic::AtomicU64;
use shuttle::sync::atomic::Ordering;
use shuttle::thread;

#[derive(Clone, Copy)]
enum Clear {
    /// What the checkpoint did before the fix: test the lock, release it, then compare and swap.
    AfterTheTest,
    /// What it does: clear the words while it still holds the lock.
    UnderTheLock,
}

/// One slot: its liveness lock, which a dead owner left free, and its generation word, which
/// the dead owner left set.
struct Slot {
    liveness: Mutex<()>,
    generation: AtomicU64,
}

const GENERATION: u64 = 5;

fn checkpoint(slot: &Slot, clear: Clear) {
    let generation = slot.generation.load(Ordering::SeqCst);
    if generation == 0 {
        return;
    }
    match clear {
        Clear::AfterTheTest => {
            let dead = slot.liveness.try_lock().is_ok();
            if dead {
                let _ = slot.generation.compare_exchange(
                    generation,
                    0,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                );
            }
        }
        Clear::UnderTheLock => {
            if let Ok(_held) = slot.liveness.try_lock() {
                slot.generation.store(0, Ordering::SeqCst);
            }
        }
    }
}

/// A dead process left the slot claimed. A joiner claims it with the same generation while a
/// checkpoint clears dead slots, and once both are done the joiner's generation must remain.
fn reclaim(clear: Clear) {
    let slot = Arc::new(Slot {
        liveness: Mutex::new(()),
        generation: AtomicU64::new(GENERATION),
    });
    let joiner = {
        let slot = Arc::clone(&slot);
        thread::spawn(move || {
            let held = slot.liveness.lock().unwrap();
            slot.generation.store(GENERATION, Ordering::SeqCst);
            thread::yield_now();
            assert_eq!(
                slot.generation.load(Ordering::SeqCst),
                GENERATION,
                "a checkpoint cleared the slot of a live process"
            );
            drop(held);
        })
    };
    let checkpointer = {
        let slot = Arc::clone(&slot);
        thread::spawn(move || checkpoint(&slot, clear))
    };
    joiner.join().unwrap();
    checkpointer.join().unwrap();
}

#[test]
#[ignore = "left out of mise run check, run by mise run random"]
#[should_panic]
fn clearing_after_releasing_the_tested_lock_clears_a_live_joiner() {
    shuttle::check_random(|| reclaim(Clear::AfterTheTest), schedules());
}

#[test]
#[ignore = "left out of mise run check, run by mise run random"]
fn clearing_under_the_liveness_lock_keeps_every_live_joiner() {
    shuttle::check_random(|| reclaim(Clear::UnderTheLock), schedules());
    shuttle::check_pct(|| reclaim(Clear::UnderTheLock), schedules(), 3);
}

/// 10000 schedules, times the scale that `RHIZODB_TEST_SCALE` sets for nightly runs.
fn schedules() -> usize {
    let scale = std::env::var("RHIZODB_TEST_SCALE")
        .ok()
        .and_then(|scale| scale.parse::<usize>().ok())
        .unwrap_or(1);
    10_000 * scale.max(1)
}
