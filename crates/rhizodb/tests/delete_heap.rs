//! The peak heap of a DETACH DELETE grows with the elements it deletes, not with the rows that
//! name them. The scenario runs in one test, since other tests on other threads would move the
//! count.

use std::alloc::GlobalAlloc;
use std::alloc::Layout;
use std::alloc::System;
use std::sync::atomic::AtomicIsize;
use std::sync::atomic::Ordering;

use rhizodb::Database;
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

#[test]
fn detach_deleting_a_hub_named_by_every_matched_row_reads_its_adjacency_once() {
    let db = Database::in_memory().unwrap();
    let followers = 3000;
    db.execute(
        &format!(
            "CREATE (h:Hub) WITH h UNWIND range(1, {followers}) AS i \
             CREATE (:Fan)-[:FOLLOWS]->(h)"
        ),
        &Parameters::new(),
    )
    .unwrap();
    let before = LIVE.load(Ordering::Relaxed);
    PEAK.store(before, Ordering::Relaxed);
    db.execute(
        "MATCH (h:Hub)<-[:FOLLOWS]-(f) DETACH DELETE h",
        &Parameters::new(),
    )
    .unwrap();
    let growth = PEAK.load(Ordering::Relaxed) - before;
    assert!(
        growth < 16 << 20,
        "the delete's heap peaked {growth} bytes above its start"
    );
    let left = db
        .execute(
            "MATCH (n) OPTIONAL MATCH (n)-[r]-() RETURN count(DISTINCT n), count(r)",
            &Parameters::new(),
        )
        .unwrap();
    assert_eq!(
        left.rows()[0],
        vec![Value::Integer(followers), Value::Integer(0)]
    );
}
