use super::*;
use crate::scale::Scale;

#[test]
fn a_tiny_replication_run_installs_a_matching_copy_and_measures_every_figure() {
    let dir = std::env::temp_dir().join(format!("rhizodb-replication-test-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let data = Dataset::generate(7, Scale::Tiny.plan());
    let report = run_replication(&dir, &data, 2).unwrap();
    fs::remove_dir_all(&dir).unwrap();
    assert!(report.stream_bytes > 0);
    assert!(report.export_mib_per_second > 0.0);
    assert!(report.install_mib_per_second > 0.0);
    assert!(report.commit_p99_alone >= report.commit_p50_alone);
    assert!(report.commit_p99_exporting >= report.commit_p50_exporting);
    let table = replication_table(&report);
    assert!(table.contains("install"));
    let document = replication_json(&report, json!({"scale": "tiny"}));
    assert_eq!(document["stream_bytes"], json!(report.stream_bytes));
}

#[test]
fn percentiles_read_the_nearest_rank() {
    let samples: Vec<Duration> = (1..=100).map(Duration::from_millis).collect();
    assert_eq!(percentile(samples.clone(), 0.5), Duration::from_millis(51));
    assert_eq!(percentile(samples, 0.99), Duration::from_millis(99));
}
