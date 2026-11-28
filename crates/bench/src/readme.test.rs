use serde_json::json;

use super::*;

fn report() -> Value {
    json!({
        "scale": "medium",
        "reps": 5,
        "persons": 20000,
        "knows": 2000000,
        "cities": 100,
        "results": [
            {"workload": "read/point-lookup", "engine": "rhizodb", "status": "measured", "median_ms": 2.0, "min_ms": 1.9, "max_ms": 2.1, "bytes": null},
            {"workload": "read/point-lookup", "engine": "ladybugdb", "status": "measured", "median_ms": 5.0, "min_ms": 4.9, "max_ms": 5.1, "bytes": null},
            {"workload": "traverse/1-hop", "engine": "rhizodb", "status": "measured", "median_ms": 30.0, "min_ms": 29.0, "max_ms": 31.0, "bytes": null},
            {"workload": "traverse/1-hop", "engine": "ladybugdb", "status": "measured", "median_ms": 15.0, "min_ms": 14.0, "max_ms": 16.0, "bytes": null},
            {"workload": "index/vector-search", "engine": "rhizodb", "status": "measured", "median_ms": 4.0, "min_ms": 3.9, "max_ms": 4.1, "bytes": null},
            {"workload": "index/vector-search", "engine": "ladybugdb", "status": "unavailable", "median_ms": null, "bytes": null},
            {"workload": "write/bulk-update", "engine": "rhizodb", "status": "measured", "median_ms": 16.5, "min_ms": 15.8, "max_ms": 19.2, "bytes": null},
            {"workload": "write/bulk-update", "engine": "ladybugdb", "status": "measured", "median_ms": 16.1, "min_ms": 13.5, "max_ms": 19.8, "bytes": null},
            {"workload": "load/bulk", "engine": "rhizodb", "status": "measured", "median_ms": 1500.0, "min_ms": 1450.0, "max_ms": 1550.0, "bytes": 1048576},
            {"workload": "load/bulk", "engine": "ladybugdb", "status": "measured", "median_ms": 900.0, "min_ms": 880.0, "max_ms": 920.0, "bytes": 3145728}
        ]
    })
}

#[test]
fn a_row_names_the_faster_engine_whichever_it_is() {
    let table = render(&report(), "4 CPUs, Linux", "2026-11-25").unwrap_or_default();

    assert!(
        table.contains("| Point lookup by id | 2.00 | 5.00 | RhizoDB 2.5x |"),
        "{table}"
    );
    assert!(
        table.contains("| 1-hop neighbours | 30.0 | 15.0 | LadybugDB 2.0x |"),
        "{table}"
    );
    assert!(
        table.contains("| Bulk load | 1500 | 900 | LadybugDB 1.7x |"),
        "{table}"
    );
}

#[test]
fn a_row_whose_ranges_overlap_names_no_winner() {
    let table = render(&report(), "4 CPUs, Linux", "2026-11-25").unwrap_or_default();

    assert!(
        table.contains("| Bulk update | 16.5 | 16.1 | similar |"),
        "{table}"
    );
}

#[test]
fn an_engine_that_did_not_measure_shows_why_and_no_winner() {
    let table = render(&report(), "4 CPUs, Linux", "2026-11-25").unwrap_or_default();

    assert!(
        table.contains("| Vector search | 4.00 | unavailable | n/a |"),
        "{table}"
    );
    assert!(
        table.contains("| Filtered scan | not run | not run | n/a |"),
        "{table}"
    );
}

#[test]
fn the_size_row_compares_the_files_after_bulk_load() {
    let table = render(&report(), "4 CPUs, Linux", "2026-11-25").unwrap_or_default();

    assert!(
        table.contains("| File size after bulk load | 1.0 MiB | 3.0 MiB | RhizoDB 3.0x smaller |"),
        "{table}"
    );
}

#[test]
fn the_heading_names_the_machine_date_scale_and_repetitions() {
    let table = render(&report(), "4 CPUs, Linux", "2026-11-25").unwrap_or_default();

    assert!(
        table.starts_with("Measured on 2026-11-25 on 4 CPUs, Linux, at the `medium` scale (20000 persons, 2000000 `KNOWS` edges, 100 cities), with 5 repetitions"),
        "{table}"
    );
}

#[test]
fn splicing_replaces_only_the_text_between_the_markers() {
    let readme = format!("intro\n{START}\nold table\n{END}\noutro\n");

    assert_eq!(
        splice(&readme, "new table\n"),
        Ok(format!("intro\n{START}\nnew table\n{END}\noutro\n"))
    );
}

#[test]
fn splicing_a_readme_without_markers_fails() {
    assert!(splice("no markers here\n", "table\n").is_err());
}

#[test]
fn a_report_without_results_fails() {
    assert!(render(&json!({}), "machine", "date").is_err());
}

#[test]
fn a_korean_table_uses_korean_words_for_every_row() {
    let table =
        render_in(&report(), "4 CPUs, Linux", "2026-11-25", Language::Korean).unwrap_or_default();

    assert!(
        table.contains("| 작업 | RhizoDB | LadybugDB | 더 빠른 쪽 |"),
        "{table}"
    );
    assert!(
        table.contains("| id로 노드 조회 | 2.00 | 5.00 | RhizoDB 2.5배 |"),
        "{table}"
    );
    assert!(
        table.contains("| 일괄 수정 | 16.5 | 16.1 | 유사 |"),
        "{table}"
    );
    assert!(
        table.contains("| 벡터 검색 | 4.00 | 지원 안 함 | 해당 없음 |"),
        "{table}"
    );
    assert!(
        table.contains("| 조건 필터 스캔 | 측정 안 함 | 측정 안 함 | 해당 없음 |"),
        "{table}"
    );
    assert!(
        table.contains("| 대량 적재 후 파일 크기 | 1.0 MiB | 3.0 MiB | RhizoDB가 3.0배 작음 |"),
        "{table}"
    );
    assert!(
        table.starts_with("2026-11-25에 4 CPUs, Linux 환경에서 측정했습니다."),
        "{table}"
    );
}

#[test]
fn every_row_label_has_a_korean_label() {
    for (_, label) in ROWS {
        assert!(
            KOREAN_LABELS.iter().any(|(english, _)| *english == label),
            "{label} has no Korean label"
        );
    }
}

#[test]
fn an_unknown_language_code_fails() {
    assert!(Language::from_code("fr").is_err());
    assert_eq!(Language::from_code("ko"), Ok(Language::Korean));
}

#[test]
fn files_whose_sizes_round_to_the_same_ratio_are_similar() {
    let mut document = report();
    for row in document["results"].as_array_mut().into_iter().flatten() {
        if row["workload"] == "load/bulk" {
            row["bytes"] = json!(if row["engine"] == "rhizodb" {
                13_484_032
            } else {
                13_631_488
            });
        }
    }
    let english = render(&document, "4 CPUs, Linux", "2026-11-25").unwrap_or_default();
    let korean =
        render_in(&document, "4 CPUs, Linux", "2026-11-25", Language::Korean).unwrap_or_default();

    assert!(
        english.contains("| File size after bulk load | 12.9 MiB | 13.0 MiB | similar |"),
        "{english}"
    );
    assert!(korean.contains("| 유사 |"), "{korean}");
}
