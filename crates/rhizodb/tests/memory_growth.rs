//! Live heap bytes, counted by a global allocator, must level off over long loops of work that
//! keeps no new data: queries with new texts, failing queries, aborted and dropped
//! transactions, reopening a file, index churn, overwriting the same property, and lookups of
//! names that do not exist. Every scenario runs in one test, since other tests on other threads
//! would move the count.

mod support;

use std::alloc::GlobalAlloc;
use std::alloc::Layout;
use std::alloc::System;
use std::sync::atomic::AtomicIsize;
use std::sync::atomic::Ordering;

use rhizodb::Database;
use rhizodb::IndexTarget;
use rhizodb::Parameters;
use rhizodb::Value;
use support::cases;

struct Counting;

static LIVE: AtomicIsize = AtomicIsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            LIVE.fetch_add(layout.size() as isize, Ordering::Relaxed);
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
            LIVE.fetch_add(size as isize - layout.size() as isize, Ordering::Relaxed);
        }
        moved
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn live() -> isize {
    LIVE.load(Ordering::Relaxed)
}

fn none() -> Parameters {
    Parameters::new()
}

/// Runs `step` for a warm-up, then for as many steps again in two halves, and fails when the
/// second half grew the live heap by more than `slack` bytes, since a bounded cache may fill in the
/// first.
fn levels_off(name: &str, steps: u64, slack: isize, mut step: impl FnMut(u64)) -> String {
    for i in 0..steps {
        step(i);
    }
    let start = live();
    for i in steps..2 * steps {
        step(i);
    }
    let middle = live();
    for i in 2 * steps..3 * steps {
        step(i);
    }
    let end = live();
    let (first, second) = (middle - start, end - middle);
    let report = format!("{name}: {first} then {second} bytes over {steps} steps each");
    assert!(second <= slack, "{report}");
    report
}

/// RhizoDB's own engine keeps an in-memory database's log in memory until a checkpoint, which
/// starts once the log passes 256 MiB or the memtable 64 MiB, so commits grow the heap up to that.
const ENGINE_LOG_AND_MEMTABLE: isize = 320 << 20;

#[test]
#[ignore = "left out of mise run check, run by mise run random"]
fn the_heap_levels_off_over_long_loops_that_keep_no_new_data() {
    let steps = cases(300);
    let quiet = 256 << 10;
    let committing = quiet + ENGINE_LOG_AND_MEMTABLE;
    let mut reports = Vec::new();

    let mut leak = Vec::new();
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        levels_off("a deliberate leak", steps, quiet, |_| {
            leak.push(vec![0u8; 4096])
        });
    }));
    drop(leak);
    assert!(caught.is_err(), "a leak of 4 KiB per step passes");

    let db = support::db();
    db.execute(
        "UNWIND range(1, 50) AS i INSERT (:P {k: i, name: 'n' + toString(i)})",
        &none(),
    )
    .unwrap();
    reports.push(levels_off("new query texts", steps, quiet, |i| {
        db.execute(
            &format!("MATCH (n:P) WHERE n.k = {} RETURN n.name, {i} AS i", i % 50),
            &none(),
        )
        .unwrap();
    }));
    reports.push(levels_off("failing queries", steps, quiet, |i| {
        let _ = db.execute(&format!("MATCH (n:P) RETURN n.k / {}", i % 2), &none());
        let _ = db.execute(&format!("MATCH (n:P RETURN {i}"), &none());
        let _ = db.execute("MATCH (n:P) RETURN toInteger(n.name) + 'x'", &none());
    }));
    reports.push(levels_off(
        "rolled back and dropped writes",
        steps,
        quiet,
        |i| {
            let mut tx = db.begin_write().unwrap();
            tx.execute(
                "UNWIND range(1, 200) AS i INSERT (:Q {k: i, s: 'some text'})",
                &none(),
            )
            .unwrap();
            tx.execute("MATCH (n:P) SET n.tmp = n.k", &none()).unwrap();
            if i % 2 == 0 {
                tx.rollback().unwrap();
            } else {
                drop(tx);
            }
        },
    ));
    reports.push(levels_off(
        "a write that a failed query rolled back",
        steps,
        quiet,
        |_| {
            let mut tx = db.begin_write().unwrap();
            tx.execute("UNWIND range(1, 100) AS i INSERT (:Q {k: i})", &none())
                .unwrap();
            assert!(tx.execute("MATCH (n:P) RETURN n.k / 0", &none()).is_err());
            assert!(tx.commit().is_err());
        },
    ));
    reports.push(levels_off(
        "read transactions across commits",
        steps,
        committing,
        |i| {
            let read = db.begin_read().unwrap();
            db.execute(
                "MATCH (n:P {k: 1}) SET n.v = $v",
                &Parameters::from([("v".to_owned(), Value::Integer(i as i64))]),
            )
            .unwrap();
            read.execute("MATCH (n:P) RETURN count(n)", &none())
                .unwrap();
        },
    ));
    reports.push(levels_off(
        "overwriting one property",
        steps,
        committing,
        |i| {
            db.execute(
                "MATCH (n:P) WHERE n.k <= 10 SET n.v = $v, n.w = $w",
                &Parameters::from([
                    ("v".to_owned(), Value::Integer(i as i64)),
                    (
                        "w".to_owned(),
                        Value::String("x".repeat((i % 7) as usize * 100)),
                    ),
                ]),
            )
            .unwrap();
        },
    ));
    reports.push(levels_off(
        "inserting and deleting the same amount",
        steps,
        committing,
        |_| {
            db.execute(
                "UNWIND range(1, 20) AS i INSERT (:T {k: i})-[:E {w: i}]->(:T)",
                &none(),
            )
            .unwrap();
            db.execute("MATCH (n:T) DETACH DELETE n", &none()).unwrap();
        },
    ));
    reports.push(levels_off("names that do not exist", steps, quiet, |i| {
        db.execute(
            &format!("MATCH (n:Missing{i})-[:NoType{i}]->(m) RETURN n.nokey{i}"),
            &none(),
        )
        .unwrap();
    }));
    reports.push(levels_off(
        "creating and dropping indexes",
        steps / 3,
        committing,
        |i| {
            let name = format!("index{}", i % 3);
            db.create_property_index(&name, IndexTarget::Label("P"), "k")
                .unwrap();
            db.create_keyword_index(&format!("kw{name}"), IndexTarget::Label("P"), &["name"])
                .unwrap();
            db.execute(
                "CALL db.index.keyword.queryNodes($i, 'n1', 5) YIELD node RETURN node.k",
                &Parameters::from([("i".to_owned(), Value::String(format!("kw{name}")))]),
            )
            .unwrap();
            assert!(db.drop_index(&name).unwrap());
            assert!(db.drop_index(&format!("kw{name}")).unwrap());
        },
    ));
    reports.push(levels_off(
        "threads that each run a query",
        steps / 3,
        quiet,
        |_| {
            std::thread::scope(|scope| {
                scope.spawn(|| db.execute("MATCH (n:P) RETURN count(*)", &none()).unwrap());
            });
        },
    ));
    drop(db);

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("reopened.rhizodb");
    Database::open(&path)
        .unwrap()
        .execute("UNWIND range(1, 500) AS i INSERT (:P {k: i})", &none())
        .unwrap();
    reports.push(levels_off("reopening a file", steps / 3, committing, |i| {
        let db = Database::open(&path).unwrap();
        db.execute(
            "MATCH (n:P) WHERE n.k = $k SET n.v = $k",
            &Parameters::from([("k".to_owned(), Value::Integer(i as i64 % 500))]),
        )
        .unwrap();
    }));
    eprintln!("{}", reports.join("\n"));
}
