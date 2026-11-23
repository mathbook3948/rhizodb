mod support;

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use rhizodb::Database;
use rhizodb::ErrorKind;
use rhizodb::Parameters;
use support::*;

/// Runs `work` on its own thread and fails when it does not finish within `limit`, so a hang
/// fails the test instead of the whole run. A hung thread is left behind.
fn finishes_within<T: Send + 'static>(
    limit: Duration,
    work: impl FnOnce() -> T + Send + 'static,
) -> T {
    let (done, finished) = mpsc::channel();
    thread::spawn(move || {
        let _ = done.send(work());
    });
    finished
        .recv_timeout(limit)
        .unwrap_or_else(|_| panic!("the work did not finish within {limit:?}"))
}

/// A thread that keeps opening and committing empty write transactions, so the writer slot
/// changes hands with every round of the thread that races it.
fn committer(db: &Arc<Database>, stop: &Arc<AtomicBool>) -> thread::JoinHandle<()> {
    let (db, stop) = (Arc::clone(db), Arc::clone(stop));
    thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            db.begin_write().unwrap().commit().unwrap();
        }
    })
}

#[test]
#[ignore = "bug: a committing writer clears the writer slot after redb already handed the write \
            lock to the next thread, so that thread's second begin_write on itself waits forever \
            instead of failing with WriteTransactionOnThisThread"]
fn a_second_write_on_the_writing_thread_fails_even_right_after_another_thread_committed() {
    let db = Arc::new(Database::in_memory().unwrap());
    let stop = Arc::new(AtomicBool::new(false));
    let other = committer(&db, &stop);
    let rounds = Arc::new(AtomicU64::new(0));
    let (done, finished) = mpsc::channel();
    {
        let (db, rounds) = (Arc::clone(&db), Arc::clone(&rounds));
        thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(20);
            while Instant::now() < deadline {
                let tx = db.begin_write().unwrap();
                let nested = db.begin_write().map(drop).map_err(|error| error.kind());
                if nested != Err(ErrorKind::WriteTransactionOnThisThread) {
                    let _ = done.send(Err(format!("nested begin_write gave {nested:?}")));
                    return;
                }
                drop(tx);
                rounds.fetch_add(1, Ordering::Relaxed);
            }
            let _ = done.send(Ok(()));
        });
    }
    let outcome = finished.recv_timeout(Duration::from_secs(40));
    stop.store(true, Ordering::Relaxed);
    let rounds = rounds.load(Ordering::Relaxed);
    match outcome {
        Ok(Ok(())) => other.join().unwrap(),
        Ok(Err(problem)) => panic!("after {rounds} rounds: {problem}"),
        Err(_) => panic!("after {rounds} rounds the nested begin_write waited for its own thread"),
    }
}

#[test]
fn writers_on_many_threads_each_commit_whole_and_none_waits_forever() {
    let db = Arc::new(Database::in_memory().unwrap());
    let threads = 8;
    let rounds = cases(40);
    let shared = Arc::clone(&db);
    finishes_within(Duration::from_secs(120), move || {
        thread::scope(|scope| {
            for writer in 0..threads {
                let db = &shared;
                scope.spawn(move || {
                    for round in 0..rounds {
                        let parameters = Parameters::from([
                            ("w".to_owned(), int(writer)),
                            ("r".to_owned(), int(round as i64)),
                        ]);
                        if round % 3 == 0 {
                            db.execute(
                                "INSERT (:W {w: $w, r: $r}), (:W {w: $w, r: $r})",
                                &parameters,
                            )
                            .unwrap();
                            continue;
                        }
                        let mut tx = db.begin_write().unwrap();
                        tx.execute("INSERT (:W {w: $w, r: $r})", &parameters)
                            .unwrap();
                        tx.execute("INSERT (:W {w: $w, r: $r})", &parameters)
                            .unwrap();
                        if round % 3 == 1 {
                            tx.commit().unwrap();
                        } else {
                            tx.rollback().unwrap();
                        }
                    }
                });
            }
        });
    });
    let committed = (0..rounds).filter(|round| round % 3 != 2).count() as i64;
    assert_eq!(
        run(&db, "MATCH (n:W) RETURN n.w, count(*) ORDER BY n.w"),
        (0..threads)
            .map(|writer| vec![int(writer), int(2 * committed)])
            .collect::<Vec<_>>()
    );
    assert_eq!(
        single(
            &db,
            "MATCH (n:W) WITH n.w AS w, n.r AS r, count(*) AS c WHERE c <> 2 RETURN count(*)"
        ),
        int(0)
    );
}

#[test]
fn a_waiting_writer_proceeds_when_the_holder_rolls_back_fails_or_is_dropped() {
    for ending in ["rollback", "failed query", "drop"] {
        let db = Arc::new(Database::in_memory().unwrap());
        let mut holder = db.begin_write().unwrap();
        holder
            .execute("INSERT (:Held)", &Parameters::new())
            .unwrap();
        let (began, waited) = mpsc::channel();
        let waiter = {
            let db = Arc::clone(&db);
            thread::spawn(move || {
                db.execute("INSERT (:Waiter)", &Parameters::new()).unwrap();
                began.send(()).unwrap();
            })
        };
        assert!(
            waited.recv_timeout(Duration::from_millis(100)).is_err(),
            "{ending}"
        );
        match ending {
            "rollback" => holder.rollback().unwrap(),
            "failed query" => {
                assert!(holder.execute("RETURN 1 / 0", &Parameters::new()).is_err());
                assert_eq!(
                    holder
                        .execute("RETURN 1", &Parameters::new())
                        .unwrap_err()
                        .kind(),
                    ErrorKind::TransactionAborted
                );
                drop(holder);
            }
            _ => drop(holder),
        }
        waited.recv_timeout(Duration::from_secs(10)).unwrap();
        waiter.join().unwrap();
        assert_eq!(
            run(&db, "MATCH (n) RETURN labels(n)"),
            [vec![list([string("Waiter")])]],
            "{ending}"
        );
    }
}

#[test]
fn readers_keep_their_snapshot_while_writers_on_other_threads_commit_and_drop_indexes() {
    let db = Arc::new(Database::in_memory().unwrap());
    db.create_property_index("by_i", rhizodb::IndexTarget::Label("N"), "i")
        .unwrap();
    run(&db, "UNWIND range(1, 50) AS i INSERT (:N {i: i})");
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let (db, stop) = (Arc::clone(&db), Arc::clone(&stop));
        thread::spawn(move || {
            let mut round = 0_i64;
            while !stop.load(Ordering::Relaxed) {
                let mut tx = db.begin_write().unwrap();
                tx.execute(
                    "MATCH (n:N) WITH n ORDER BY n.i LIMIT 1 SET n.i = n.i + 50",
                    &Parameters::new(),
                )
                .unwrap();
                tx.commit().unwrap();
                if round % 5 == 0 {
                    db.drop_index("by_i").unwrap();
                    db.create_property_index("by_i", rhizodb::IndexTarget::Label("N"), "i")
                        .unwrap();
                }
                round += 1;
            }
        })
    };
    let readers: Vec<_> = (0..3)
        .map(|_| {
            let db = Arc::clone(&db);
            thread::spawn(move || {
                for _ in 0..cases(60) {
                    let snapshot = db.begin_read().unwrap();
                    let read = |query: &str| {
                        snapshot
                            .execute(query, &Parameters::new())
                            .unwrap()
                            .rows()
                            .to_vec()
                    };
                    let all = read("MATCH (n:N) RETURN n.i ORDER BY n.i");
                    assert_eq!(all.len(), 50);
                    let low = all[0][0].clone();
                    for _ in 0..3 {
                        assert_eq!(read("MATCH (n:N) RETURN n.i ORDER BY n.i"), all);
                        let parameters = Parameters::from([("i".to_owned(), low.clone())]);
                        let found = snapshot
                            .execute("MATCH (n:N {i: $i}) RETURN n.i", &parameters)
                            .unwrap();
                        assert_eq!(found.rows(), [vec![low.clone()]]);
                    }
                }
            })
        })
        .collect();
    for reader in readers {
        reader.join().unwrap();
    }
    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();
}
