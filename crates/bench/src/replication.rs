//! Replication costs on RhizoDB alone: how fast a snapshot of a loaded graph is read out and
//! installed, and how much an export in progress slows the writer's commits. Every installed copy
//! is checked against the source's counts, so a fast but wrong copy fails the run.

use std::fs;
use std::fs::File;
use std::io;
use std::io::BufReader;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use rhizodb::Database;
use rhizodb::Parameters;
use rhizodb::Value;
use serde_json::Value as Json;
use serde_json::json;

use crate::data::Dataset;
use crate::engine::Engine;
use crate::rhizo::FILE_NAME;
use crate::rhizo::RhizoEngine;

const READ_BUFFER: usize = 1 << 20;
const COMMITS: usize = 400;
const COUNT: &str = "MATCH (n) RETURN count(n)";

/// The medians of one replication run over its repetitions.
#[derive(Clone, Debug, PartialEq)]
pub struct ReplicationReport {
    pub stream_bytes: u64,
    pub export_mib_per_second: f64,
    pub install_mib_per_second: f64,
    pub commit_p50_alone: Duration,
    pub commit_p99_alone: Duration,
    pub commit_p50_exporting: Duration,
    pub commit_p99_exporting: Duration,
    pub reps: usize,
}

fn failed(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}

fn median_f64(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    values.get(values.len() / 2).copied().unwrap_or(0.0)
}

fn median(mut values: Vec<Duration>) -> Duration {
    values.sort();
    values.get(values.len() / 2).copied().unwrap_or_default()
}

fn percentile(mut values: Vec<Duration>, share: f64) -> Duration {
    values.sort();
    let at = ((values.len() as f64 - 1.0) * share).round() as usize;
    values.get(at).copied().unwrap_or_default()
}

fn mib_per_second(bytes: u64, elapsed: Duration) -> f64 {
    bytes as f64 / (1u64 << 20) as f64 / elapsed.as_secs_f64().max(1e-9)
}

fn node_count(db: &Database) -> io::Result<Value> {
    let result = db.execute(COUNT, &Parameters::new()).map_err(failed)?;
    Ok(result.rows()[0][0].clone())
}

fn export_to(db: &Database, path: &Path) -> io::Result<(u64, Duration)> {
    let started = Instant::now();
    let snapshot = db.snapshot().map_err(failed)?;
    let mut out = File::create(path)?;
    let mut buffer = vec![0; READ_BUFFER];
    let mut offset = 0;
    loop {
        let read = snapshot.read_at(offset, &mut buffer).map_err(failed)?;
        if read == 0 {
            break;
        }
        out.write_all(&buffer[..read])?;
        offset += read as u64;
    }
    out.sync_all()?;
    Ok((offset, started.elapsed()))
}

fn commit_latencies(db: &Database, base: i64) -> io::Result<Vec<Duration>> {
    let mut latencies = Vec::with_capacity(COMMITS);
    for index in 0..COMMITS {
        let parameters = Parameters::from([("i".to_owned(), Value::Integer(base + index as i64))]);
        let started = Instant::now();
        db.execute("INSERT (:Tick {i: $i})", &parameters)
            .map_err(failed)?;
        latencies.push(started.elapsed());
    }
    Ok(latencies)
}

fn commits_while_exporting(db: &Database, base: i64) -> io::Result<Vec<Duration>> {
    let snapshot = db.snapshot().map_err(failed)?;
    let done = AtomicBool::new(false);
    thread::scope(|scope| {
        let reader = scope.spawn(|| {
            let mut buffer = vec![0; READ_BUFFER];
            let mut offset = 0;
            while !done.load(Ordering::Relaxed) {
                let read = snapshot.read_at(offset, &mut buffer).map_err(failed)?;
                offset = if read == 0 { 0 } else { offset + read as u64 };
            }
            Ok::<(), io::Error>(())
        });
        let latencies = commit_latencies(db, base);
        done.store(true, Ordering::Relaxed);
        reader
            .join()
            .map_err(|_| failed("the export reader panicked"))??;
        latencies
    })
}

/// Loads the dataset into a RhizoDB file under `dir`, then for each repetition exports a
/// snapshot to a file, installs it beside the source, checks the copy, and measures the
/// writer's commit latency with and without an export running, alternating which goes first.
pub fn run_replication(dir: &Path, data: &Dataset, reps: usize) -> io::Result<ReplicationReport> {
    let source_dir = dir.join("source");
    fs::create_dir_all(&source_dir)?;
    {
        let instance = RhizoEngine.open(&source_dir).map_err(failed)?;
        instance.create_schema().map_err(failed)?;
        instance.prepare_bulk_load(data, dir).map_err(failed)?;
        instance.bulk_load(data, dir).map_err(failed)?;
    }
    let source = Database::open(source_dir.join(FILE_NAME)).map_err(failed)?;
    source.compact().map_err(failed)?;
    let mut export_rates = Vec::new();
    let mut install_rates = Vec::new();
    let mut alone = (Vec::new(), Vec::new());
    let mut exporting = (Vec::new(), Vec::new());
    let mut stream_bytes = 0;
    let stream = dir.join("snapshot.stream");
    let replica = dir.join("replica.rhizodb");
    for rep in 0..reps {
        let expected = node_count(&source)?;
        let (bytes, elapsed) = export_to(&source, &stream)?;
        stream_bytes = bytes;
        export_rates.push(mib_per_second(bytes, elapsed));
        let started = Instant::now();
        let installed = Database::install_snapshot(&replica, BufReader::new(File::open(&stream)?))
            .map_err(failed)?;
        install_rates.push(mib_per_second(bytes, started.elapsed()));
        if node_count(&installed)? != expected {
            return Err(failed(
                "the installed copy holds other nodes than the source",
            ));
        }
        drop(installed);
        fs::remove_file(&replica)?;
        let base = (rep * 2 * COMMITS) as i64 + 1_000_000_000;
        let (first, second) = if rep % 2 == 0 {
            let plain = commit_latencies(&source, base)?;
            (
                plain,
                commits_while_exporting(&source, base + COMMITS as i64)?,
            )
        } else {
            let busy = commits_while_exporting(&source, base)?;
            (commit_latencies(&source, base + COMMITS as i64)?, busy)
        };
        alone.0.push(percentile(first.clone(), 0.5));
        alone.1.push(percentile(first, 0.99));
        exporting.0.push(percentile(second.clone(), 0.5));
        exporting.1.push(percentile(second, 0.99));
        source.compact().map_err(failed)?;
    }
    Ok(ReplicationReport {
        stream_bytes,
        export_mib_per_second: median_f64(export_rates),
        install_mib_per_second: median_f64(install_rates),
        commit_p50_alone: median(alone.0),
        commit_p99_alone: median(alone.1),
        commit_p50_exporting: median(exporting.0),
        commit_p99_exporting: median(exporting.1),
        reps,
    })
}

/// The report as a table for the terminal.
pub fn replication_table(report: &ReplicationReport) -> String {
    format!(
        "snapshot stream: {:.1} MiB, medians of {} repetitions\n\
         export: {:.1} MiB/s\ninstall: {:.1} MiB/s\n\
         commit p50 alone: {:?}, while exporting: {:?}\n\
         commit p99 alone: {:?}, while exporting: {:?}\n",
        report.stream_bytes as f64 / (1u64 << 20) as f64,
        report.reps,
        report.export_mib_per_second,
        report.install_mib_per_second,
        report.commit_p50_alone,
        report.commit_p50_exporting,
        report.commit_p99_alone,
        report.commit_p99_exporting,
    )
}

/// The report as JSON, under `header`.
pub fn replication_json(report: &ReplicationReport, header: Json) -> Json {
    json!({
        "header": header,
        "stream_bytes": report.stream_bytes,
        "export_mib_per_second": report.export_mib_per_second,
        "install_mib_per_second": report.install_mib_per_second,
        "commit_p50_alone_us": report.commit_p50_alone.as_micros() as u64,
        "commit_p99_alone_us": report.commit_p99_alone.as_micros() as u64,
        "commit_p50_exporting_us": report.commit_p50_exporting.as_micros() as u64,
        "commit_p99_exporting_us": report.commit_p99_exporting.as_micros() as u64,
    })
}

#[cfg(test)]
#[path = "replication.test.rs"]
mod tests;
