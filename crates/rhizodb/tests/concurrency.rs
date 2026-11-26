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
#[ignore = "left out of mise run check, run by mise run random"]
fn a_second_write_on_the_writing_thread_fails_even_right_after_another_thread_committed() {
    let db = Arc::new(db());
    let stop = Arc::new(AtomicBool::new(false));
    let other = committer(&db, &stop);
    let rounds = Arc::new(AtomicU64::new(0));
    let (done, finished) = mpsc::channel();
    {
        let (db, rounds) = (Arc::clone(&db), Arc::clone(&rounds));
        thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(cases(4).min(60));
            while Instant::now() < deadline {
                let tx = db.begin_write().unwrap();
                let nested = if rounds.load(Ordering::Relaxed) % 2 == 0 {
                    db.begin_write().map(drop)
                } else {
                    db.execute("INSERT (:Nested)", &Parameters::new()).map(drop)
                }
                .map_err(|error| error.kind());
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
    let outcome = finished.recv_timeout(Duration::from_secs(cases(4).min(60) + 30));
    stop.store(true, Ordering::Relaxed);
    let rounds = rounds.load(Ordering::Relaxed);
    match outcome {
        Ok(Ok(())) => other.join().unwrap(),
        Ok(Err(problem)) => panic!("after {rounds} rounds: {problem}"),
        Err(_) => panic!("after {rounds} rounds the nested begin_write waited for its own thread"),
    }
}

#[test]
#[ignore = "left out of mise run check, run by mise run random"]
fn writers_on_many_threads_each_commit_whole_and_none_waits_forever() {
    let db = Arc::new(db());
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
        let db = Arc::new(db());
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
    let db = Arc::new(db());
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

#[test]
fn readers_see_whole_adjacency_snapshots_while_a_writer_moves_relationships_in_small_commits() {
    let db = Arc::new(db());
    run(&db, "INSERT (:Hub)");
    run(&db, "UNWIND range(1, 200) AS i INSERT (:N {i: i})");
    run(
        &db,
        "MATCH (h:Hub), (n:N) WHERE n.i <= 100 INSERT (h)-[:T]->(n), (n)-[:T]->(h)",
    );
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let (db, stop) = (Arc::clone(&db), Arc::clone(&stop));
        thread::spawn(move || {
            let mut round = 0_i64;
            while !stop.load(Ordering::Relaxed) {
                let target = round % 200 + 1;
                let mut tx = db.begin_write().unwrap();
                tx.execute(
                    "MATCH (h:Hub)-[r:T]->(n) WITH r ORDER BY id(r) LIMIT 1 DELETE r",
                    &Parameters::new(),
                )
                .unwrap();
                tx.execute(
                    "MATCH (n:N)-[r:T]->(:Hub) WITH r ORDER BY id(r) LIMIT 1 DELETE r",
                    &Parameters::new(),
                )
                .unwrap();
                tx.execute(
                    "MATCH (h:Hub), (n:N {i: $i}) INSERT (h)-[:T]->(n), (n)-[:T]->(h)",
                    &Parameters::from([("i".to_owned(), rhizodb::Value::Integer(target))]),
                )
                .unwrap();
                tx.commit().unwrap();
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
                    let out = read("MATCH (h:Hub)-[r:T]->(n) RETURN id(r), n.i ORDER BY id(r)");
                    assert_eq!(out.len(), 100);
                    let back = read("MATCH (n:N)<-[r:T]-(h:Hub) RETURN id(r), n.i ORDER BY id(r)");
                    assert_eq!(back, out, "both ends list the same relationships");
                    let into = read("MATCH (h:Hub)<-[r]-() RETURN count(r)");
                    assert_eq!(into, [vec![int(100)]]);
                    for _ in 0..2 {
                        assert_eq!(
                            read("MATCH (h:Hub)-[r:T]->(n) RETURN id(r), n.i ORDER BY id(r)"),
                            out
                        );
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
    assert_eq!(
        run(&db, "MATCH ()-[r:T]->() RETURN count(r)"),
        [vec![int(200)]]
    );
}
