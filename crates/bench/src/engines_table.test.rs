use serde_json::json;

use super::*;

fn row(workload: &str, engine: &str, median: f64, min: f64, max: f64) -> Value {
    json!({"workload": workload, "engine": engine, "status": "measured", "median_ms": median, "min_ms": min, "max_ms": max, "bytes": null})
}

fn report() -> Value {
    json!({
        "scale": "medium",
        "reps": 5,
        "persons": 20000,
        "knows": 200000,
        "cities": 100,
        "results": [
            row("read/point-lookup", "rhizodb", 2.0, 1.9, 2.1),
            row("read/point-lookup", "sqlite", 1.0, 0.9, 1.1),
            row("read/point-lookup", "duckdb", 9.0, 8.0, 10.0),
            row("traverse/1-hop", "rhizodb", 30.0, 29.0, 31.0),
            row("traverse/1-hop", "sqlite", 31.0, 30.5, 33.0),
            row("traverse/1-hop", "duckdb", 90.0, 80.0, 100.0),
            {"workload": "index/vector-search", "engine": "rhizodb", "status": "measured", "median_ms": 4.0, "min_ms": 3.9, "max_ms": 4.1, "bytes": null},
            {"workload": "index/vector-search", "engine": "sqlite", "status": "unsupported", "median_ms": null, "bytes": null},
            {"workload": "index/vector-search", "engine": "duckdb", "status": "unavailable", "median_ms": null, "bytes": null},
            {"workload": "load/bulk", "engine": "rhizodb", "status": "measured", "median_ms": 800.0, "min_ms": 780.0, "max_ms": 820.0, "bytes": 2097152},
            {"workload": "load/bulk", "engine": "sqlite", "status": "measured", "median_ms": 400.0, "min_ms": 390.0, "max_ms": 410.0, "bytes": 3145728},
            {"workload": "load/bulk", "engine": "duckdb", "status": "measured", "median_ms": 600.0, "min_ms": 590.0, "max_ms": 610.0, "bytes": 1048576}
        ]
    })
}

fn table() -> String {
    render(&report(), "4 CPUs, Linux", "2026-11-28").unwrap_or_default()
}

#[test]
fn the_table_has_a_column_for_each_engine_of_the_report_in_report_order() {
    assert!(
        table().contains("| Workload | RhizoDB | SQLite | DuckDB | Fastest |\n| --- | ---: | ---: | ---: | --- |\n"),
        "{}",
        table()
    );
}

#[test]
fn a_row_names_the_fastest_engine_and_how_far_ahead_of_the_next_it_is() {
    assert!(
        table().contains("| Point lookup by id | 2.00 | 1.00 | 9.00 | SQLite 2.0x |"),
        "{}",
        table()
    );
    assert!(
        table().contains("| Bulk load | 800 | 400 | 600 | SQLite 1.5x |"),
        "{}",
        table()
    );
}

#[test]
fn a_row_whose_two_fastest_overlap_names_no_winner() {
    assert!(
        table().contains("| 1-hop neighbours | 30.0 | 31.0 | 90.0 | similar |"),
        "{}",
        table()
    );
}

#[test]
fn an_engine_that_did_not_measure_shows_why_and_a_workload_none_ran_has_no_row() {
    assert!(
        table().contains("| Vector search | 4.00 | unsupported | unavailable | RhizoDB |"),
        "{}",
        table()
    );
    assert!(!table().contains("| Filtered scan |"), "{}", table());
}

#[test]
fn the_size_row_names_the_smallest_file_after_bulk_load() {
    assert!(
        table().contains(
            "| File size after bulk load | 2.0 MiB | 3.0 MiB | 1.0 MiB | DuckDB 2.0x smaller |"
        ),
        "{}",
        table()
    );
}

#[test]
fn splicing_replaces_only_the_text_between_the_markers() {
    let document = format!("intro\n{START}\nold table\n{END}\noutro\n");

    assert_eq!(
        splice_between(&document, "new table\n", START, END),
        Ok(format!("intro\n{START}\nnew table\n{END}\noutro\n"))
    );
}

#[test]
fn a_report_without_results_fails() {
    assert!(render(&json!({}), "machine", "date").is_err());
}

#[test]
fn sizes_less_than_five_percent_apart_are_similar_and_six_percent_apart_are_not() {
    let size_cell = |rhizodb: u64, sqlite: u64| {
        let document = json!({
            "results": [
                {"workload": "load/bulk", "engine": "rhizodb", "status": "measured", "median_ms": 1.0, "min_ms": 1.0, "max_ms": 1.0, "bytes": rhizodb},
                {"workload": "load/bulk", "engine": "sqlite", "status": "measured", "median_ms": 1.0, "min_ms": 1.0, "max_ms": 1.0, "bytes": sqlite}
            ]
        });
        render(&document, "machine", "2026-11-28")
            .unwrap_or_default()
            .lines()
            .last()
            .unwrap_or_default()
            .rsplit('|')
            .nth(1)
            .unwrap_or_default()
            .trim()
            .to_owned()
    };

    assert_eq!(size_cell(1_000_000, 1_049_000), "similar");
    assert_eq!(size_cell(1_000_000, 1_060_000), "RhizoDB 1.1x smaller");
}
