use std::fs;

use serde_json::Value;

/// The line in README.md after which the generated table starts.
pub const START: &str = "<!-- bench-table:start -->";

/// The line in README.md before which the generated table ends.
pub const END: &str = "<!-- bench-table:end -->";

/// The engine every row compares with RhizoDB.
pub const OTHER: &str = "ladybugdb";

/// The language a README is written in, which sets the words of its table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Language {
    /// README.md.
    English,
    /// README.ko.md.
    Korean,
}

impl Language {
    /// Reads the language from its code: `en` or `ko`.
    pub fn from_code(code: &str) -> Result<Self, String> {
        match code {
            "en" => Ok(Self::English),
            "ko" => Ok(Self::Korean),
            other => Err(format!(
                "unknown README language `{other}`, expected `en` or `ko`"
            )),
        }
    }

    fn label(self, english: &'static str) -> &'static str {
        if self == Self::English {
            return english;
        }
        KOREAN_LABELS
            .iter()
            .find(|(from, _)| *from == english)
            .map_or(english, |(_, korean)| korean)
    }
}

const KOREAN_LABELS: [(&str, &str); 18] = [
    ("Point lookup by id", "id로 노드 조회"),
    ("Filtered scan", "조건 필터 스캔"),
    ("`ORDER BY ... LIMIT`", "`ORDER BY ... LIMIT`"),
    ("1-hop neighbours", "1홉 이웃"),
    (
        "1-hop with a relationship property",
        "1홉 이웃과 관계 속성 읽기",
    ),
    ("Sum of a relationship property", "관계 속성 합계"),
    ("2-hop distinct", "2홉 이웃(중복 제거)"),
    ("3-hop distinct", "3홉 이웃(중복 제거)"),
    ("Variable length `*1..3`", "가변 길이 `*1..3`"),
    ("Group by city", "도시별 집계"),
    (
        "Substring scan of a text property",
        "텍스트 속성 부분 문자열 검색",
    ),
    ("Keyword search", "키워드 검색"),
    ("Vector search", "벡터 검색"),
    ("Small commits", "작은 커밋"),
    ("Bulk update", "일괄 수정"),
    ("Detach delete", "`DETACH DELETE`"),
    ("Bulk load", "대량 적재"),
    ("File size after bulk load", "대량 적재 후 파일 크기"),
];

/// The workloads README.md shows, in order, each with the label of its row.
pub const ROWS: [(&str, &str); 17] = [
    ("read/point-lookup", "Point lookup by id"),
    ("read/filter-scan", "Filtered scan"),
    ("read/order-limit", "`ORDER BY ... LIMIT`"),
    ("traverse/1-hop", "1-hop neighbours"),
    ("traverse/1-hop-since", "1-hop with a relationship property"),
    ("scan/knows-since-sum", "Sum of a relationship property"),
    ("traverse/2-hop-distinct", "2-hop distinct"),
    ("traverse/3-hop-distinct", "3-hop distinct"),
    ("traverse/variable-length-1..3", "Variable length `*1..3`"),
    ("aggregate/group-by-city", "Group by city"),
    (
        "scan/bio-contains-term",
        "Substring scan of a text property",
    ),
    ("index/keyword-search", "Keyword search"),
    ("index/vector-search", "Vector search"),
    ("write/small-commits", "Small commits"),
    ("write/bulk-update", "Bulk update"),
    ("write/detach-delete", "Detach delete"),
    ("load/bulk", "Bulk load"),
];

/// The workload whose on-disk size the size row shows.
pub const SIZE_FROM: &str = "load/bulk";

fn result<'a>(document: &'a Value, workload: &str, engine: &str) -> Option<&'a Value> {
    document["results"].as_array()?.iter().find(|row| {
        row["workload"].as_str() == Some(workload) && row["engine"].as_str() == Some(engine)
    })
}

fn milliseconds(value: f64) -> String {
    if value >= 100.0 {
        format!("{value:.0}")
    } else if value >= 10.0 {
        format!("{value:.1}")
    } else if value >= 1.0 {
        format!("{value:.2}")
    } else {
        format!("{value:.3}")
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Timing {
    median: f64,
    min: f64,
    max: f64,
}

fn cell(row: Option<&Value>, language: Language) -> (String, Option<Timing>) {
    let Some(row) = row else {
        let text = match language {
            Language::English => "not run",
            Language::Korean => "측정 안 함",
        };
        return (text.to_owned(), None);
    };
    let timing = (|| {
        Some(Timing {
            median: row["median_ms"].as_f64()?,
            min: row["min_ms"].as_f64()?,
            max: row["max_ms"].as_f64()?,
        })
    })();
    match timing {
        Some(timing) if row["status"].as_str() == Some("measured") => {
            (milliseconds(timing.median), Some(timing))
        }
        _ => {
            let status = row["status"].as_str().unwrap_or("failed");
            let text = match (language, status) {
                (Language::Korean, "failed") => "실패",
                (Language::Korean, "unavailable") => "지원 안 함",
                _ => status,
            };
            (text.to_owned(), None)
        }
    }
}

fn not_applicable(language: Language) -> String {
    match language {
        Language::English => "n/a",
        Language::Korean => "해당 없음",
    }
    .to_owned()
}

fn faster(rhizodb: Option<Timing>, other: Option<Timing>, language: Language) -> String {
    match (rhizodb, other) {
        (Some(ours), Some(theirs)) if ours.min <= theirs.max && theirs.min <= ours.max => {
            match language {
                Language::English => "similar",
                Language::Korean => "유사",
            }
            .to_owned()
        }
        (Some(Timing { median: ours, .. }), Some(Timing { median: theirs, .. }))
            if ours > 0.0 && theirs > 0.0 =>
        {
            let (engine, ratio) = if ours <= theirs {
                ("RhizoDB", theirs / ours)
            } else {
                ("LadybugDB", ours / theirs)
            };
            match language {
                Language::English => format!("{engine} {ratio:.1}x"),
                Language::Korean => format!("{engine} {ratio:.1}배"),
            }
        }
        _ => not_applicable(language),
    }
}

fn mebibytes(row: Option<&Value>, language: Language) -> (String, Option<f64>) {
    match row.and_then(|row| row["bytes"].as_f64()) {
        Some(bytes) => (format!("{:.1} MiB", bytes / 1_048_576.0), Some(bytes)),
        None => (not_applicable(language), None),
    }
}

fn smaller(rhizodb: Option<f64>, other: Option<f64>, language: Language) -> String {
    match (rhizodb, other) {
        (Some(ours), Some(theirs)) if ours > 0.0 && theirs > 0.0 => {
            let (engine, ratio) = if ours <= theirs {
                ("RhizoDB", theirs / ours)
            } else {
                ("LadybugDB", ours / theirs)
            };
            if format!("{ratio:.1}") == "1.0" {
                return match language {
                    Language::English => "similar".to_owned(),
                    Language::Korean => "유사".to_owned(),
                };
            }
            match language {
                Language::English => format!("{engine} {ratio:.1}x smaller"),
                Language::Korean => format!("{engine}가 {ratio:.1}배 작음"),
            }
        }
        _ => not_applicable(language),
    }
}

/// Renders the English README table from the JSON report the benchmark binary writes with
/// `--out`, naming the machine and the date of the run.
pub fn render(document: &Value, machine: &str, date: &str) -> Result<String, String> {
    render_in(document, machine, date, Language::English)
}

/// Renders the README table in `language` from the JSON report the benchmark binary writes
/// with `--out`, naming the machine and the date of the run.
pub fn render_in(
    document: &Value,
    machine: &str,
    date: &str,
    language: Language,
) -> Result<String, String> {
    if !document["results"].is_array() {
        return Err("the report has no results".to_owned());
    }
    let scale = document["scale"].as_str().unwrap_or("unknown");
    let reps = document["reps"].as_u64().unwrap_or(0);
    let persons = document["persons"].as_u64().unwrap_or(0);
    let knows = document["knows"].as_u64().unwrap_or(0);
    let cities = document["cities"].as_u64().unwrap_or(0);
    let mut text = match language {
        Language::English => format!(
            "Measured on {date} on {machine}, at the `{scale}` scale ({persons} persons, {knows} `KNOWS` edges, {cities} cities), with {reps} repetitions per workload. Times are medians in milliseconds for the whole workload, lower is better, and a row whose ranges of runs overlap is marked similar.\n\n"
        ),
        Language::Korean => format!(
            "{date}, {machine}에서 측정했습니다. 규모는 `{scale}`(사람 {persons}명, `KNOWS` 관계 {knows}개, 도시 {cities}개)이고 작업마다 {reps}번 반복했습니다. 값은 작업 전체 시간의 중앙값(밀리초)이며 낮을수록 빠릅니다. 반복 측정 범위가 두 엔진에서 겹치면 '유사'로 표시합니다.\n\n"
        ),
    };
    text.push_str(match language {
        Language::English => "| Workload | RhizoDB | LadybugDB | Faster |\n",
        Language::Korean => "| 작업 | RhizoDB | LadybugDB | 더 빠른 쪽 |\n",
    });
    text.push_str("| --- | ---: | ---: | --- |\n");
    for (workload, label) in ROWS {
        let (ours, ours_ms) = cell(result(document, workload, "rhizodb"), language);
        let (theirs, theirs_ms) = cell(result(document, workload, OTHER), language);
        let winner = faster(ours_ms, theirs_ms, language);
        let label = language.label(label);
        text.push_str(&format!("| {label} | {ours} | {theirs} | {winner} |\n"));
    }
    let (ours, ours_bytes) = mebibytes(result(document, SIZE_FROM, "rhizodb"), language);
    let (theirs, theirs_bytes) = mebibytes(result(document, SIZE_FROM, OTHER), language);
    let winner = smaller(ours_bytes, theirs_bytes, language);
    let label = language.label("File size after bulk load");
    text.push_str(&format!("| {label} | {ours} | {theirs} | {winner} |\n"));
    Ok(text)
}

/// Replaces the text between [`START`] and [`END`] in `readme` with `table`.
pub fn splice(readme: &str, table: &str) -> Result<String, String> {
    let start = readme
        .find(START)
        .ok_or_else(|| format!("README.md has no `{START}` line"))?;
    let end = readme
        .find(END)
        .filter(|end| *end > start)
        .ok_or_else(|| format!("README.md has no `{END}` line after `{START}`"))?;
    Ok(format!(
        "{}{START}\n{table}{}",
        &readme[..start],
        &readme[end..]
    ))
}

/// Reads the report at the first argument and rewrites the table in the README at the second,
/// naming the machine of the third argument and the date of the fourth, in the language whose
/// code is the optional fifth argument, English by default.
pub fn main(arguments: impl IntoIterator<Item = String>) -> Result<(), String> {
    let arguments: Vec<String> = arguments.into_iter().collect();
    let (report, readme, machine, date, language) = match arguments.as_slice() {
        [report, readme, machine, date] => (report, readme, machine, date, Language::English),
        [report, readme, machine, date, code] => {
            (report, readme, machine, date, Language::from_code(code)?)
        }
        _ => {
            return Err(
                "usage: readme-table REPORT.json README.md MACHINE DATE [en|ko]".to_owned(),
            );
        }
    };
    let report_text =
        fs::read_to_string(report).map_err(|error| format!("cannot read {report}: {error}"))?;
    let document: Value = serde_json::from_str(&report_text)
        .map_err(|error| format!("{report} is not a benchmark report: {error}"))?;
    let readme_text =
        fs::read_to_string(readme).map_err(|error| format!("cannot read {readme}: {error}"))?;
    let table = render_in(&document, machine, date, language)?;
    let updated = splice(&readme_text, &table)?;
    fs::write(readme, updated).map_err(|error| format!("cannot write {readme}: {error}"))
}

#[cfg(test)]
#[path = "readme.test.rs"]
mod tests;
