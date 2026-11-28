use std::cell::Cell;

use super::*;

#[test]
fn a_lone_query_takes_every_other_thread_as_a_helper() {
    let workers = Workers::default();
    let _running = workers.enter();

    let helpers = workers.reserve(3, 4);

    assert_eq!(helpers.threads, 3);
}

#[test]
fn queries_that_keep_every_thread_busy_leave_no_helpers() {
    let workers = Workers::default();
    let running: Vec<Busy<'_>> = (0..4).map(|_| workers.enter()).collect();

    assert_eq!(workers.reserve(3, 4).threads, 0);
    drop(running);
    let _running = workers.enter();
    assert_eq!(workers.reserve(3, 4).threads, 3);
}

#[test]
fn helpers_count_as_busy_until_their_split_ends() {
    let workers = Workers::default();
    let _first = workers.enter();
    let _second = workers.enter();

    let helpers = workers.reserve(3, 4);
    assert_eq!(helpers.threads, 2);
    assert_eq!(workers.reserve(3, 4).threads, 0);
    drop(helpers);
    assert_eq!(workers.reserve(3, 4).threads, 2);
}

thread_local! {
    static RAN: Cell<Option<(usize, usize)>> = const { Cell::new(None) };
}

/// Notes, for the calling thread, how many helpers the split it ran took and how many morsels
/// with start nodes its workers read.
pub(crate) fn ran<P>(helpers: usize, finished: &[(usize, Result<Option<P>, Error>)]) {
    let read = finished
        .iter()
        .filter(|(_, part)| !matches!(part, Ok(None)))
        .count();
    RAN.with(|ran| ran.set(Some((helpers, read))));
}

const GROUPS: usize = 20;
const NODES: usize = GROUPS * crate::graph::SLOTS;

fn databases(threshold: usize, setup: &[String]) -> (crate::Database, crate::Database) {
    let open = |options: crate::OpenOptions| {
        let db = options.in_memory().unwrap();
        for statement in setup {
            db.execute(statement, &crate::Parameters::new()).unwrap();
        }
        db
    };
    (
        open(crate::OpenOptions::new().threads(1)),
        open(
            crate::OpenOptions::new()
                .threads(4)
                .parallel_threshold(threshold),
        ),
    )
}

fn numbered(threshold: usize) -> (crate::Database, crate::Database) {
    databases(
        threshold,
        &[format!(
            "UNWIND range(0, {}) AS i INSERT (:P {{id: i}})",
            NODES - 1
        )],
    )
}

fn outcome(db: &crate::Database, query: &str) -> String {
    match db.execute(query, &crate::Parameters::new()) {
        Ok(result) => format!("{:?}", result.rows()),
        Err(error) => format!("error {:?}: {error}", error.kind()),
    }
}

fn split_run(
    (serial, split): &(crate::Database, crate::Database),
    query: &str,
) -> (String, Option<(usize, usize)>) {
    let alone = outcome(serial, query);
    RAN.with(|ran| ran.set(None));
    let parallel = outcome(split, query);
    assert_eq!(alone, parallel, "{query}");
    (parallel, RAN.with(Cell::take))
}

#[test]
fn a_limit_whose_rows_come_late_reads_row_groups_on_several_threads() {
    let dbs = numbered(2);
    let late = NODES - 2;

    let (rows, ran) = split_run(
        &dbs,
        &format!("MATCH (n:P) WHERE n.id >= {late} RETURN n.id LIMIT 3"),
    );

    assert_eq!(
        rows,
        format!("[[Integer({late})], [Integer({})]]", late + 1)
    );
    let (helpers, read) = ran.expect("the match split");
    assert!(helpers > 0, "took no helper");
    assert_eq!(read, GROUPS);
}

#[test]
fn a_limit_its_first_row_group_fills_takes_no_helpers() {
    let dbs = numbered(2);

    let (rows, ran) = split_run(&dbs, "MATCH (n:P) RETURN n.id SKIP 1 LIMIT 2");

    assert_eq!(rows, "[[Integer(1)], [Integer(2)]]");
    assert_eq!(ran, Some((0, 1)));
}

#[test]
fn a_limit_found_by_the_caller_alone_stops_reading_row_groups() {
    let second = crate::graph::SLOTS;
    let dbs = numbered(3 * second);

    let (rows, ran) = split_run(
        &dbs,
        &format!(
            "MATCH (n:P) WHERE n.id >= {second} AND n.id < {} RETURN n.id LIMIT 2",
            second + 3
        ),
    );

    assert_eq!(
        rows,
        format!("[[Integer({second})], [Integer({})]]", second + 1)
    );
    assert_eq!(ran, Some((0, 2)));
}

#[test]
fn a_limit_over_rows_that_may_fail_still_fails_on_a_late_row() {
    let dbs = numbered(2);
    let late = NODES - 2;

    let (outcome, _) = split_run(
        &dbs,
        &format!("MATCH (n:P) WHERE 1 / (n.id - {late}) <> 7 RETURN n.id LIMIT 2"),
    );

    assert_eq!(outcome, "error DivisionByZero: integer division by zero");
}

#[test]
fn a_split_limit_skips_row_groups_without_start_nodes() {
    let slots = crate::graph::SLOTS;
    let dbs = databases(
        2,
        &[
            format!("UNWIND range(0, {}) AS i INSERT (:Q {{id: i}})", NODES - 1),
            format!(
                "MATCH (n:Q) WHERE n.id % {slots} < 2 AND (n.id / {slots}) IN [3, 11, 12, 19] \
                 SET n:P REMOVE n:Q"
            ),
            format!("MATCH (n) WHERE n.id / {slots} = 12 DETACH DELETE n"),
        ],
    );

    let (rows, ran) = split_run(
        &dbs,
        &format!("MATCH (n:P) WHERE n.id > {} RETURN n.id LIMIT 3", 4 * slots),
    );
    let (_, read) = ran.expect("the match split");

    assert_eq!(
        rows,
        format!(
            "[[Integer({})], [Integer({})], [Integer({})]]",
            11 * slots,
            11 * slots + 1,
            19 * slots
        )
    );
    assert_eq!(read, 3);
}

#[test]
fn a_split_takes_one_thread_for_each_half_threshold_of_start_nodes() {
    let threshold = 2 * crate::graph::SLOTS;
    for (groups, helpers) in [(5, 4), (GROUPS, 19)] {
        let db = crate::OpenOptions::new()
            .threads(20)
            .parallel_threshold(threshold)
            .in_memory()
            .unwrap();
        db.execute(
            &format!(
                "UNWIND range(0, {}) AS i INSERT (:P {{id: i}})",
                groups * crate::graph::SLOTS - 1
            ),
            &crate::Parameters::new(),
        )
        .unwrap();
        RAN.with(|ran| ran.set(None));
        let rows = outcome(&db, "MATCH (n:P) WHERE n.id % 2 = 0 RETURN count(*)");

        assert_eq!(
            rows,
            format!("[[Integer({})]]", groups * crate::graph::SLOTS / 2)
        );
        assert_eq!(
            RAN.with(Cell::take).map(|(taken, _)| taken),
            Some(helpers),
            "{groups} groups"
        );
    }
}
