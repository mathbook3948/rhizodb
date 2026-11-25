//! One writer commits large values, transfers between two accounts, and deletes old batches, so
//! RhizoDB's own engine checkpoints in the background, while reader threads hold snapshots
//! across those commits and checkpoints. Every snapshot must show one committed state whole, and
//! the same answers each time it is read, however long it stays open.

mod support;

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use rhizodb::Database;
use rhizodb::IndexTarget;
use rhizodb::Parameters;
use rhizodb::Value;
use support::*;

const KEEP: i64 = 30;
const READERS: usize = 3;

fn size_of(i: i64) -> i64 {
    (i * 7919) % 1_000_000
}

fn parameters(entries: &[(&str, Value)]) -> Parameters {
    entries
        .iter()
        .map(|(name, value)| ((*name).to_owned(), value.clone()))
        .collect()
}

fn write_iteration(db: &Database, i: i64) {
    let mut tx = db.begin_write().unwrap();
    tx.execute(
        "INSERT (:Batch {i: $i, payload: $payload})",
        &parameters(&[
            ("i", Value::Integer(i)),
            ("payload", Value::String("p".repeat(size_of(i) as usize))),
        ]),
    )
    .unwrap();
    tx.execute(
        "MATCH (a:Account {name: 'a'}), (b:Account {name: 'b'}) SET a.v = a.v - $i, b.v = b.v + $i",
        &parameters(&[("i", Value::Integer(i))]),
    )
    .unwrap();
    tx.execute(
        "MATCH (b:Batch) WHERE b.i <= $old DELETE b",
        &parameters(&[("old", Value::Integer(i - KEEP))]),
    )
    .unwrap();
    tx.execute(
        "MATCH (c:Counter) SET c.n = $n",
        &parameters(&[("n", Value::Integer(i + 1))]),
    )
    .unwrap();
    tx.commit().unwrap();
}

/// What one snapshot shows, read in one go.
fn observe(read: &rhizodb::ReadTransaction<'_>) -> Result<Vec<Vec<Value>>, String> {
    let none = Parameters::new();
    let mut rows = Vec::new();
    for query in [
        "MATCH (c:Counter) RETURN c.n",
        "MATCH (a:Account) RETURN sum(a.v), count(a)",
        "MATCH (b:Batch) RETURN b.i, size(b.payload) ORDER BY b.i",
        "MATCH (b:Batch) WHERE b.i >= 0 RETURN count(b)",
    ] {
        let result = read
            .execute(query, &none)
            .map_err(|error| format!("{query}: {error}"))?;
        rows.extend(result.rows().iter().cloned());
    }
    Ok(rows)
}

fn check(rows: &[Vec<Value>]) -> Result<(), String> {
    let [Value::Integer(n)] = rows[0].as_slice() else {
        return Err(format!("counter {:?}", rows[0]));
    };
    if rows[1] != [Value::Integer(0), Value::Integer(2)] {
        return Err(format!("accounts {:?} at {n}", rows[1]));
    }
    let first = (n - KEEP).max(0);
    let batches = &rows[2..rows.len() - 1];
    let want: Vec<Vec<Value>> = (first..*n)
        .map(|i| vec![Value::Integer(i), Value::Integer(size_of(i))])
        .collect();
    if batches != want.as_slice() {
        return Err(format!(
            "batches at {n}: got {} rows, want {}",
            batches.len(),
            want.len()
        ));
    }
    if rows[rows.len() - 1] != [Value::Integer(n - first)] {
        return Err(format!(
            "indexed batch count {:?} at {n}",
            rows[rows.len() - 1]
        ));
    }
    Ok(())
}

fn stress(db: Arc<Database>, seconds: u64) {
    db.create_property_index("batch_i", IndexTarget::Label("Batch"), "i")
        .unwrap();
    run(
        &db,
        "INSERT (:Counter {n: 0}), (:Account {name: 'a', v: 0}), (:Account {name: 'b', v: 0})",
    );
    let stop = Arc::new(AtomicBool::new(false));
    let snapshots = Arc::new(AtomicU64::new(0));
    let readers: Vec<_> = (0..READERS)
        .map(|reader| {
            let (db, stop, snapshots) =
                (Arc::clone(&db), Arc::clone(&stop), Arc::clone(&snapshots));
            thread::spawn(move || {
                let mut rng = Rng::new(seed().wrapping_add(reader as u64));
                while !stop.load(Ordering::Relaxed) {
                    let read = db.begin_read().unwrap();
                    let first = observe(&read).unwrap();
                    check(&first).unwrap();
                    for _ in 0..rng.below(4) {
                        thread::sleep(Duration::from_millis(rng.below(200)));
                        let again = observe(&read).unwrap();
                        assert!(again == first, "a snapshot changed while it was open");
                    }
                    snapshots.fetch_add(1, Ordering::Relaxed);
                }
            })
        })
        .collect();
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut i = 0;
    while Instant::now() < deadline {
        write_iteration(&db, i);
        i += 1;
    }
    stop.store(true, Ordering::Relaxed);
    for reader in readers {
        reader.join().unwrap();
    }
    check(&observe(&db.begin_read().unwrap()).unwrap()).unwrap();
    eprintln!(
        "{i} commits, {} snapshots",
        snapshots.load(Ordering::Relaxed)
    );
}

#[test]
fn snapshots_held_across_commits_and_checkpoints_stay_whole_in_memory() {
    stress(Arc::new(db()), cases(2));
}

#[test]
fn snapshots_held_across_commits_and_checkpoints_stay_whole_on_a_file() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("snapshots.rhizodb");
    stress(Arc::new(Database::open(&path).unwrap()), cases(2));
    let db = Database::open(&path).unwrap();
    check(&observe(&db.begin_read().unwrap()).unwrap()).unwrap();
}
