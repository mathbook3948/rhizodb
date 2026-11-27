//! The peak heap of a query whose rows stream holds per-group state and the rows a LIMIT keeps,
//! sorted or not, not every row its source produces. The scenarios run in one test, since other tests on other
//! threads would move the count.

use std::alloc::GlobalAlloc;
use std::alloc::Layout;
use std::alloc::System;
use std::sync::atomic::AtomicIsize;
use std::sync::atomic::Ordering;

use rhizodb::Database;
use rhizodb::Parameters;

struct Peak;

static LIVE: AtomicIsize = AtomicIsize::new(0);
static PEAK: AtomicIsize = AtomicIsize::new(0);

fn grow(bytes: isize) {
    let live = LIVE.fetch_add(bytes, Ordering::Relaxed) + bytes;
    PEAK.fetch_max(live, Ordering::Relaxed);
}

unsafe impl GlobalAlloc for Peak {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            grow(layout.size() as isize);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
        LIVE.fetch_sub(layout.size() as isize, Ordering::Relaxed);
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let moved = unsafe { System.realloc(pointer, layout, size) };
        if !moved.is_null() {
            grow(size as isize - layout.size() as isize);
        }
        moved
    }
}

#[global_allocator]
static ALLOCATOR: Peak = Peak;

fn peak_growth(db: &Database, text: &str) -> (isize, Vec<Vec<rhizodb::Value>>) {
    let before = LIVE.load(Ordering::Relaxed);
    PEAK.store(before, Ordering::Relaxed);
    let result = db.execute(text, &Parameters::new()).unwrap();
    (
        PEAK.load(Ordering::Relaxed) - before,
        result.rows().to_vec(),
    )
}

#[test]
#[ignore = "left out of mise run check, run by mise run random"]
fn streamed_rows_hold_group_state_not_every_input_row() {
    let db = Database::in_memory().unwrap();
    db.execute(
        "UNWIND range(0, 49999) AS i CREATE (:P {id: i})",
        &Parameters::new(),
    )
    .unwrap();
    let integer = rhizodb::Value::Integer;
    for (text, rows, bound) in [
        (
            "UNWIND range(1, 2000000) AS i RETURN count(*)",
            vec![vec![integer(2_000_000)]],
            1 << 20,
        ),
        (
            "UNWIND [2000000] AS n UNWIND range(1, n) AS i RETURN i % 3 AS k, count(*) ORDER BY k",
            vec![
                vec![integer(0), integer(666_666)],
                vec![integer(1), integer(666_667)],
                vec![integer(2), integer(666_667)],
            ],
            1 << 20,
        ),
        (
            "UNWIND range(1, 2000) AS j CALL { UNWIND range(1, 1000) AS i RETURN i } \
             WITH i RETURN sum(i)",
            vec![vec![integer(2000 * 1000 * 1001 / 2)]],
            1 << 20,
        ),
        (
            "UNWIND range(1, 2000000) AS i RETURN i ORDER BY -i LIMIT 3",
            vec![
                vec![integer(2_000_000)],
                vec![integer(1_999_999)],
                vec![integer(1_999_998)],
            ],
            1 << 20,
        ),
        (
            "UNWIND range(1, 2000000) AS i RETURN i, i % 1000 AS k ORDER BY k DESC SKIP 2 LIMIT 2",
            vec![
                vec![integer(2999), integer(999)],
                vec![integer(3999), integer(999)],
            ],
            1 << 20,
        ),
        (
            "MATCH (a:P) RETURN a.id ORDER BY a.id % 7, a.id DESC LIMIT 2",
            vec![vec![integer(49_994)], vec![integer(49_987)]],
            4 << 20,
        ),
        (
            "MATCH (a:P) MATCH (b:P) RETURN a.id, b.id LIMIT 3",
            vec![
                vec![integer(0), integer(0)],
                vec![integer(0), integer(1)],
                vec![integer(0), integer(2)],
            ],
            4 << 20,
        ),
    ] {
        let (growth, found) = peak_growth(&db, text);
        assert_eq!(found, rows, "{text}");
        assert!(
            growth < bound,
            "{text} peaked {growth} bytes above its start, over {bound}"
        );
    }
}
