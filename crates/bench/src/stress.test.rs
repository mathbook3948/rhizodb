use super::*;
use crate::scale::Scale;

fn ms(value: u64) -> Duration {
    Duration::from_millis(value)
}

#[test]
fn latency_reads_nearest_rank_percentiles() {
    let mut samples: Vec<Duration> = (1..=100).rev().map(ms).collect();

    let latency = Latency::of(&mut samples);

    assert_eq!(latency.count, 100);
    assert_eq!(latency.p50, ms(50));
    assert_eq!(latency.p99, ms(99));
    assert_eq!(latency.max, ms(100));
    assert_eq!(Latency::of(&mut []), Latency::default());
}

#[test]
fn scenarios_parse_by_name_or_all() {
    assert_eq!(
        Scenario::parse_list("readers,sustained"),
        Some(vec![Scenario::Readers, Scenario::Sustained])
    );
    assert_eq!(Scenario::parse_list("all"), Some(Scenario::ALL.to_vec()));
    assert_eq!(Scenario::parse_list("readers,nope"), None);
}

#[test]
fn reading_scenarios_run_once_per_thread_count_and_sustained_once() {
    let load = Load {
        scenarios: Scenario::ALL.to_vec(),
        threads: vec![1, 4],
        duration: ms(1),
    };

    let labels: Vec<String> = Point::all(&load).into_iter().map(Point::label).collect();

    assert_eq!(
        labels,
        [
            "readers/1-threads",
            "readers/4-threads",
            "mixed/1-threads",
            "mixed/4-threads",
            "sustained"
        ]
    );
}

#[test]
fn answers_count_distinct_two_hop_targets_and_aged_persons() {
    let data = Dataset::generate(7, Scale::Tiny.plan());

    let answers = Answers::of(&data);

    let probe = data.probes[0];
    let mut reached = BTreeSet::new();
    for first in data.knows.iter().filter(|edge| edge.from == probe) {
        for second in data.knows.iter().filter(|edge| edge.from == first.to) {
            reached.insert(second.to);
        }
    }
    let mut row = RowHasher::default();
    row.int(reached.len() as i64);
    let mut expected = Fingerprint::default();
    expected.absorb(row);
    assert_eq!(answers.expected(Read::TwoHop(probe)), expected);
    let aged = data
        .people
        .iter()
        .filter(|person| person.age.is_some_and(|age| age > 30))
        .count() as i64;
    assert_eq!(answers.scan, aged);
}

#[test]
fn the_mix_draws_every_kind_of_read() {
    let data = Dataset::generate(7, Scale::Tiny.plan());
    let answers = Answers::of(&data);
    let mut random = Random::new(1);

    let kinds: BTreeSet<&str> = (0..200).map(|_| answers.draw(&mut random).kind()).collect();

    assert_eq!(
        kinds,
        BTreeSet::from(["2-hop", "filter-scan", "point-lookup"])
    );
}

#[test]
fn a_second_whose_log_shrank_is_a_checkpoint() {
    let sample = |at: u64, wal_bytes: u64| Sample {
        at: ms(at),
        wal_bytes,
        data_bytes: 0,
        rss_bytes: None,
    };
    let samples = [
        sample(0, 10),
        sample(500, 20),
        sample(1200, 5),
        sample(1900, 8),
    ];

    let seconds = timeline(ms(1900), &[], &[], &samples);

    let marked: Vec<bool> = seconds.iter().map(|second| second.checkpoint).collect();
    assert_eq!(marked, [false, true]);
}

#[test]
fn the_longest_read_gap_and_stalls_come_from_read_end_times() {
    let reader = ReaderLog {
        ops: vec![
            Op {
                end: ms(10),
                took: ms(1),
            },
            Op {
                end: ms(400),
                took: ms(300),
            },
            Op {
                end: ms(450),
                took: ms(2),
            },
        ],
        ..ReaderLog::default()
    };

    let run = summarize(ms(500), vec![reader], None, &[], None);

    assert_eq!(run.longest_read_gap, ms(390));
    assert_eq!(run.stalled_reads, 1);
    assert_eq!(run.reads.count, 3);
}

fn mixed(readers: usize, commits: u64) -> PointReport {
    let run = LoadRun {
        elapsed: Duration::from_secs(1),
        writes: Latency {
            count: commits,
            ..Latency::default()
        },
        ..LoadRun::default()
    };
    PointReport {
        point: Point {
            scenario: Scenario::Mixed,
            readers,
        },
        engines: vec![
            EngineRuns {
                engine: EngineKind::Rhizodb,
                runs: vec![run.clone()],
                failure: None,
            },
            EngineRuns {
                engine: EngineKind::Ladybugdb,
                runs: vec![LoadRun {
                    writes: Latency::default(),
                    ..run
                }],
                failure: None,
            },
        ],
    }
}

#[test]
fn a_writer_that_keeps_half_its_one_reader_commits_is_not_starved() {
    let reports = [mixed(1, 2000), mixed(16, 1200), mixed(32, 999)];

    let starved = starved_writer(&reports);

    assert_eq!(starved.len(), 1, "{starved:?}");
    assert!(starved[0].starts_with("mixed/32-threads"), "{starved:?}");
    assert!(starved_writer(&reports[..2]).is_empty());
    assert!(starved_writer(&reports[1..]).is_empty());
}
