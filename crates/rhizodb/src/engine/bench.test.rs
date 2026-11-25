use std::path::Path;
use std::time::Duration;
use std::time::Instant;

use crate::engine::any_fs::open_path;
use crate::engine::store::EngineOptions;
use crate::storage::ReadTransaction;
use crate::storage::Storage;
use crate::storage::Table;
use crate::storage::WriteTransaction;
use crate::storage::redb_backend::RedbStorage;

const KEYS: u64 = 1_000_000;
const BATCH: u64 = 10_000;
const SMALL_COMMITS: u64 = 1000;
const READS: u64 = 200_000;

fn key(id: u64) -> [u8; 16] {
    let mut key = [0; 16];
    key[..8].copy_from_slice(&(id.wrapping_mul(0x9E37_79B9_7F4A_7C15)).to_be_bytes());
    key[8..].copy_from_slice(&id.to_be_bytes());
    key
}

fn value(id: u64) -> Vec<u8> {
    (0..100).map(|at| (id as u8).wrapping_add(at)).collect()
}

fn directory_bytes(path: &Path) -> u64 {
    std::fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().metadata().unwrap().len())
        .sum()
}

fn timed(label: &str, results: &mut Vec<(String, Duration)>, work: impl FnOnce()) {
    let start = Instant::now();
    work();
    results.push((label.to_owned(), start.elapsed()));
}

fn workload<S: Storage + Sync>(store: &S, results: &mut Vec<(String, Duration)>) {
    timed("bulk load 1M keys in commits of 10k", results, || {
        for batch in 0..KEYS / BATCH {
            let mut write = store.begin_write().unwrap();
            for id in batch * BATCH..(batch + 1) * BATCH {
                write.put(Table::Nodes, &key(id), &value(id)).unwrap();
            }
            write.commit().unwrap();
        }
    });
    timed("1000 commits of 5 puts", results, || {
        for commit in 0..SMALL_COMMITS {
            let mut write = store.begin_write().unwrap();
            for item in 0..5 {
                let id = (commit * 5 + item) * 997 % KEYS;
                write.put(Table::Nodes, &key(id), &value(id + 1)).unwrap();
            }
            write.commit().unwrap();
        }
    });
    timed("200k random point reads", results, || {
        let read = store.begin_read().unwrap();
        for index in 0..READS {
            let id = index * 7_919 % KEYS;
            assert!(read.get(Table::Nodes, &key(id)).unwrap().is_some());
        }
    });
    timed("the same 200k point reads again", results, || {
        let read = store.begin_read().unwrap();
        for index in 0..READS {
            let id = index * 7_919 % KEYS;
            assert!(read.get(Table::Nodes, &key(id)).unwrap().is_some());
        }
    });
    timed("200k random point reads on 4 threads", results, || {
        read_points_concurrently(store, KEYS, 4, READS / 4);
    });
    timed("full scan of 1M keys", results, || {
        let read = store.begin_read().unwrap();
        let mut count = 0;
        read.visit_prefix(Table::Nodes, &[], |_, _| {
            count += 1;
            Ok::<_, crate::storage::StorageError>(())
        })
        .unwrap();
        assert_eq!(count, KEYS);
    });
}

#[test]
#[ignore = "a benchmark, run in release with --ignored --nocapture"]
fn the_engine_against_redb_on_kv_workloads() {
    let redb_directory = tempfile::tempdir().unwrap();
    let engine_directory = tempfile::tempdir().unwrap();
    let mut redb_results = Vec::new();
    let mut engine_results = Vec::new();
    {
        let store = RedbStorage::open(redb_directory.path().join("graph.redb")).unwrap();
        workload(&store, &mut redb_results);
    }
    let redb_size = directory_bytes(redb_directory.path());
    {
        let options = EngineOptions {
            cache_pages: std::env::var("BENCH_CACHE_PAGES")
                .ok()
                .and_then(|pages| pages.parse().ok())
                .unwrap_or(EngineOptions::default().cache_pages),
            reclaim_slack: EngineOptions::default().reclaim_slack,
            memtable_limit: std::env::var("BENCH_MEMTABLE")
                .ok()
                .and_then(|bytes| bytes.parse().ok())
                .unwrap_or(EngineOptions::default().memtable_limit),
            ..EngineOptions::default()
        };
        let store = open_path(&engine_directory.path().join("graph.rhizo"), options).unwrap();
        workload(&store, &mut engine_results);
        let report = store.check().unwrap();
        eprintln!(
            "engine check: {} pages, {} live blocks",
            report.pages, report.live_blocks
        );
    }
    let engine_size = directory_bytes(engine_directory.path());
    eprintln!("| workload | redb | engine |");
    eprintln!("|---|---|---|");
    for ((label, redb), (_, engine)) in redb_results.iter().zip(&engine_results) {
        eprintln!(
            "| {label} | {:.3} s | {:.3} s |",
            redb.as_secs_f64(),
            engine.as_secs_f64()
        );
    }
    eprintln!(
        "| size at rest | {:.1} MiB | {:.1} MiB |",
        redb_size as f64 / 1048576.0,
        engine_size as f64 / 1048576.0
    );
}

fn read_points(store: &impl Storage, keys: u64, reads: u64) {
    let read = store.begin_read().unwrap();
    for index in 0..reads {
        let id = index * 7_919 % keys;
        assert!(read.get(Table::Nodes, &key(id)).unwrap().is_some());
    }
}

fn read_points_concurrently<S: Storage + Sync>(store: &S, keys: u64, threads: u64, reads: u64) {
    std::thread::scope(|scope| {
        for thread in 0..threads {
            scope.spawn(move || {
                let read = store.begin_read().unwrap();
                for index in 0..reads {
                    let id = (index * 7_919 + thread * 104_729) % keys;
                    assert!(read.get(Table::Nodes, &key(id)).unwrap().is_some());
                }
            });
        }
    });
}

fn profile_keys() -> u64 {
    std::env::var("BENCH_KEYS")
        .ok()
        .and_then(|keys| keys.parse().ok())
        .unwrap_or(100_000)
}

fn load(store: &impl Storage, keys: u64) {
    for batch in 0..keys.div_ceil(BATCH) {
        let mut write = store.begin_write().unwrap();
        for id in batch * BATCH..((batch + 1) * BATCH).min(keys) {
            write.put(Table::Nodes, &key(id), &value(id)).unwrap();
        }
        write.commit().unwrap();
    }
}

fn profile_point_reads<S: Storage + Sync>(store: &S, keys: u64) {
    read_points(store, keys, 20_000);
    let reads = 100_000;
    let start = Instant::now();
    read_points(store, keys, reads);
    let single = start.elapsed();
    let threads = 4;
    let start = Instant::now();
    read_points_concurrently(store, keys, threads, reads);
    let concurrent = start.elapsed();
    eprintln!(
        "{keys} keys: {:.0} ns per point read, {:.0} ns per point read on {threads} threads",
        single.as_nanos() as f64 / reads as f64,
        concurrent.as_nanos() as f64 / (threads * reads) as f64
    );
}

#[test]
#[ignore = "a profiling target, run under callgrind or in release with --nocapture"]
fn point_reads_for_profiling() {
    let directory = tempfile::tempdir().unwrap();
    let keys = profile_keys();
    let store = open_path(
        &directory.path().join("graph.rhizo"),
        EngineOptions::default(),
    )
    .unwrap();
    load(&store, keys);
    store.checkpoint().unwrap();
    profile_point_reads(&store, keys);
}

#[test]
#[ignore = "a profiling target, run under callgrind or in release with --nocapture"]
fn redb_point_reads_for_profiling() {
    let directory = tempfile::tempdir().unwrap();
    let keys = profile_keys();
    let store = RedbStorage::open(directory.path().join("graph.redb")).unwrap();
    load(&store, keys);
    profile_point_reads(&store, keys);
}

#[test]
#[ignore = "a profiling target, run under callgrind or in release with --nocapture"]
fn memtable_point_reads_for_profiling() {
    let directory = tempfile::tempdir().unwrap();
    let keys = profile_keys();
    let store = open_path(
        &directory.path().join("graph.rhizo"),
        EngineOptions::default(),
    )
    .unwrap();
    load(&store, keys);
    profile_point_reads(&store, keys);
}

fn commit_small_batches(store: &impl Storage, keys: u64) {
    for commit in 0..SMALL_COMMITS {
        let mut write = store.begin_write().unwrap();
        for item in 0..5 {
            let id = (commit * 5 + item) * 997 % keys;
            write.put(Table::Nodes, &key(id), &value(id + 1)).unwrap();
        }
        write.commit().unwrap();
    }
}

#[test]
#[ignore = "a profiling target, run under callgrind"]
fn commits_for_profiling() {
    let directory = tempfile::tempdir().unwrap();
    let keys = profile_keys();
    let options = EngineOptions {
        checkpoint: crate::engine::store::CheckpointMode::Inline,
        ..EngineOptions::default()
    };
    let store = open_path(&directory.path().join("graph.rhizo"), options).unwrap();
    load(&store, keys);
    commit_small_batches(&store, keys);
    store.checkpoint().unwrap();
}
