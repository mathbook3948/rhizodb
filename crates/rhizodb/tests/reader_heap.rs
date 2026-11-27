//! The peak heap of a traversal that reaches the row groups of its end nodes in random order
//! stays within the budget of the query's column and label readers, however much column data
//! it reads. The scenarios run in one test, since other tests on other threads would move the
//! count.

use std::alloc::GlobalAlloc;
use std::alloc::Layout;
use std::alloc::System;
use std::sync::atomic::AtomicIsize;
use std::sync::atomic::Ordering;

use rhizodb::IndexTarget;
use rhizodb::OpenOptions;
use rhizodb::Parameters;
use rhizodb::Value;

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

fn peak_growth(run: impl FnOnce() -> Vec<Value>) -> (isize, Vec<Value>) {
    let before = LIVE.load(Ordering::Relaxed);
    PEAK.store(before, Ordering::Relaxed);
    let row = run();
    (PEAK.load(Ordering::Relaxed) - before, row)
}

#[test]
fn a_traversal_in_random_group_order_holds_a_bounded_part_of_the_columns_it_reads() {
    let nodes: i64 = 16 * 4096;
    let bio = 600;
    let none = Parameters::new();
    let db = OpenOptions::new().threads(1).in_memory().unwrap();
    db.execute(
        &format!(
            "UNWIND range(0, {}) AS i CREATE (:P {{id: i, bio: '{}'}})",
            nodes - 1,
            "b".repeat(bio)
        ),
        &none,
    )
    .unwrap();
    db.create_property_index("by_id", IndexTarget::Label("P"), "id")
        .unwrap();
    db.execute(
        &format!(
            "UNWIND range(0, {last}) AS i MATCH (a:P {{id: i}}), (b:P {{id: (i * 7919 + 13) % {nodes}}}) \
             CREATE (a)-[:K]->(b)",
            last = nodes - 1
        ),
        &none,
    )
    .unwrap();
    let query = "MATCH (a)-[:K]->(b:P) RETURN sum(size(b.bio))";
    let expected = vec![Value::Integer(nodes * bio as i64)];
    let columns = nodes as isize * bio as isize;
    let bound = 16 << 20;
    assert!(
        columns > 2 * bound,
        "the traversal reads {columns} bytes of columns"
    );

    let (growth, row) = peak_growth(|| db.execute(query, &none).unwrap().rows()[0].clone());
    assert_eq!(row, expected);
    assert!(
        growth < bound,
        "the query's heap peaked {growth} bytes above its start"
    );

    let mut tx = db.begin_write().unwrap();
    let (growth, row) = peak_growth(|| {
        tx.execute(query, &none).unwrap();
        tx.execute(query, &none).unwrap().rows()[0].clone()
    });
    assert_eq!(row, expected);
    assert!(
        growth < bound,
        "two queries of a write transaction peaked {growth} bytes above its start"
    );
    tx.rollback().unwrap();
}
