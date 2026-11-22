use super::*;
use crate::fingerprint::Fingerprint;

fn run(milliseconds: u64, rows: u64, checksum: u64) -> Measurement {
    Measurement {
        elapsed: Duration::from_millis(milliseconds),
        ops: 10,
        rows,
        fingerprint: Fingerprint { rows, checksum },
        ..Measurement::default()
    }
}

fn series(engine: EngineKind, runs: Vec<Measurement>) -> Series {
    Series {
        engine,
        runs,
        unsupported: None,
        failure: None,
    }
}

#[test]
fn median_of_an_odd_count_is_the_middle_sample() {
    let samples: Vec<Duration> = [30, 10, 20].map(Duration::from_millis).to_vec();
    let stats = Stats::of(&samples);

    assert_eq!(
        stats,
        Some(Stats {
            median: Duration::from_millis(20),
            min: Duration::from_millis(10),
            max: Duration::from_millis(30),
        })
    );
}

#[test]
fn median_of_an_even_count_averages_the_middle_pair() {
    let samples: Vec<Duration> = [10, 20, 40, 30].map(Duration::from_millis).to_vec();

    assert_eq!(
        Stats::of(&samples).map(|stats| stats.median),
        Some(Duration::from_millis(25))
    );
    assert_eq!(Stats::of(&[]), None);
}

#[test]
fn engines_that_return_the_same_fingerprint_agree() {
    let a = series(EngineKind::Rhizodb, vec![run(1, 3, 9), run(2, 3, 9)]);
    let b = series(EngineKind::Ladybugdb, vec![run(1, 3, 9)]);

    assert_eq!(
        agreements(&[a, b], Comparison::Exact),
        [Agreement::Agree, Agreement::Agree]
    );
}

#[test]
fn a_different_checksum_is_a_disagreement_unless_only_rows_are_compared() {
    let a = series(EngineKind::Rhizodb, vec![run(1, 3, 9)]);
    let b = series(EngineKind::Ladybugdb, vec![run(1, 3, 8)]);
    let both = [a, b];

    let exact = agreements(&both, Comparison::Exact);
    assert_eq!(exact[0], Agreement::Agree);
    assert!(matches!(exact[1], Agreement::Disagree(_)));
    assert_eq!(
        agreements(&both, Comparison::RowsOnly),
        [Agreement::Agree, Agreement::Agree]
    );
}

#[test]
fn a_different_row_count_disagrees_even_when_only_rows_are_compared() {
    let a = series(EngineKind::Rhizodb, vec![run(1, 3, 9)]);
    let b = series(EngineKind::Ladybugdb, vec![run(1, 4, 9)]);

    assert!(matches!(
        agreements(&[a, b], Comparison::RowsOnly)[1],
        Agreement::Disagree(_)
    ));
}

#[test]
fn an_engine_that_drifts_between_repetitions_disagrees_with_itself() {
    let a = series(EngineKind::Rhizodb, vec![run(1, 3, 9), run(1, 3, 7)]);
    let b = series(EngineKind::Ladybugdb, vec![run(1, 3, 9)]);

    assert!(matches!(
        agreements(&[a, b], Comparison::Exact)[0],
        Agreement::Disagree(_)
    ));
}

#[test]
fn one_engine_alone_is_not_comparable() {
    let a = series(EngineKind::Rhizodb, vec![run(1, 3, 9)]);
    let mut b = series(EngineKind::Ladybugdb, Vec::new());
    b.unsupported = Some("no extension".to_owned());

    let verdicts = agreements(&[a, b], Comparison::Exact);
    assert_eq!(verdicts[0], Agreement::Agree);
    assert!(matches!(verdicts[1], Agreement::NotComparable(_)));
}

#[test]
fn the_table_shows_a_disagreement_loudly_and_an_unsupported_reason() {
    let a = series(EngineKind::Rhizodb, vec![run(5, 3, 9)]);
    let mut b = series(EngineKind::Ladybugdb, Vec::new());
    b.unsupported = Some("no extension".to_owned());
    let skipped = WorkloadReport {
        name: "x/skipped",
        series: vec![a.clone(), b],
        agreements: vec![Agreement::Agree, Agreement::NotComparable(String::new())],
    };
    let differing = WorkloadReport {
        name: "x/differs",
        series: vec![a],
        agreements: vec![Agreement::Disagree(
            "ladybugdb returned 1, reference rhizodb returned 2".to_owned(),
        )],
    };
    let text = table(&[skipped, differing]);

    assert!(text.contains("unsupported: no extension"));
    assert!(text.contains("DISAGREEMENT in x/differs"));
}

#[test]
fn json_carries_one_result_per_series_with_its_status() {
    let a = series(EngineKind::Rhizodb, vec![run(5, 3, 9)]);
    let mut b = series(EngineKind::Ladybugdb, Vec::new());
    b.unsupported = Some("no extension".to_owned());
    let report = WorkloadReport {
        name: "x/y",
        series: vec![a, b],
        agreements: vec![Agreement::Agree, Agreement::NotComparable(String::new())],
    };
    let document = json(&[report], serde_json::json!({"scale": "tiny"}));

    assert_eq!(document["scale"], "tiny");
    assert_eq!(document["results"][0]["status"], "measured");
    assert_eq!(document["results"][0]["median_ms"], 5.0);
    assert_eq!(document["results"][1]["status"], "unsupported");
    assert_eq!(document["results"][1]["unsupported"], "no extension");
}

#[test]
fn rhizodb_is_the_reference_whatever_the_series_order() {
    let ladybug = series(EngineKind::Ladybugdb, vec![run(1, 3, 8)]);
    let rhizo = series(EngineKind::Rhizodb, vec![run(1, 3, 9)]);
    let verdicts = agreements(&[ladybug, rhizo], Comparison::Exact);

    assert!(matches!(verdicts[0], Agreement::Disagree(_)));
    assert_eq!(verdicts[1], Agreement::Agree);
}
