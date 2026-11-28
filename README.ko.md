# RhizoDB

[![CI](https://github.com/mathbook3948/rhizodb/actions/workflows/check.yml/badge.svg)](https://github.com/mathbook3948/rhizodb/actions/workflows/check.yml)
[![Crates.io](https://img.shields.io/crates/v/rhizodb.svg)](https://crates.io/crates/rhizodb)
[![Documentation](https://docs.rs/rhizodb/badge.svg)](https://docs.rs/rhizodb)
[![License](https://img.shields.io/crates/l/rhizodb)](#라이선스)

[English](README.md) | 한국어

Rust 프로그램 안에 넣어 쓰는, 스키마 없는 속성 그래프 데이터베이스. 질의는 GQL로 합니다.

RhizoDB는 서버 없이 프로그램 안에서 돕니다. 데이터베이스 하나는 파일 하나와 그 파일의 선행 기록 로그(WAL)로 이뤄지고, 메모리에만 둘 수도 있습니다.
레이블이 붙은 노드, 타입이 있는 관계, 그리고 둘 다에 붙는 속성을 저장하고, ISO GQL 질의를 처리합니다.
Cypher의 `CREATE`도 GQL의 `INSERT`로 받아들입니다.
노드 속성은 타입이 정해진 열(column)에, 관계는 압축 희소 행(CSR) 세그먼트에 저장합니다. 그래서 스캔, 집계, 다중 홉 탐색이 필요한 데이터만 읽습니다.

```rust
use rhizodb::Database;
use rhizodb::Parameters;
use rhizodb::Value;

fn main() -> Result<(), rhizodb::Error> {
    let db = Database::in_memory()?;
    db.execute(
        "INSERT (:Person {name: 'Ada'})-[:KNOWS]->(:Person {name: 'Grace'})",
        &Parameters::new(),
    )?;

    let result = db.execute(
        "MATCH (a:Person {name: $name})-[:KNOWS]->(b) RETURN b.name AS friend",
        &Parameters::from([("name", "Ada")]),
    )?;
    assert_eq!(result.columns(), ["friend"]);
    assert_eq!(result.rows(), [vec![Value::from("Grace")]]);
    Ok(())
}
```

`Database::open("graph.rhizodb")`로 열면 같은 그래프를 파일에 둡니다.
바로 돌려 볼 수 있는 예제는 [`crates/rhizodb/examples`](https://github.com/mathbook3948/rhizodb/tree/main/crates/rhizodb/examples)에 있습니다. 트랜잭션과 탐색을 쓰는 파일 기반 소셜 그래프, 키워드 검색과 벡터 검색, 프로그램이 직접 등록하는 프로시저 예제가 있습니다.
저장소를 클론한 뒤 `cargo run --example social`처럼 실행하면 됩니다.

## 상태

RhizoDB는 아직 1.0 전인 초기 개발 단계입니다. SQLite가 관계형 저장소를 프로그램 안에 넣어 주듯, 그래프 저장소를 프로그램 안에 두고 싶은 Rust 프로그램을 위해 만들었습니다.
API나 디스크 형식이 호환되지 않게 바뀌면 마이너 버전을 올리고, 그 내용을 [CHANGELOG.md](https://github.com/mathbook3948/rhizodb/blob/main/CHANGELOG.md)에 적습니다.
예전 버전이 만든 데이터베이스는 열 때 그 자리에서 업그레이드됩니다.
이것이 백업에 어떤 의미인지는 [업그레이드 안내](https://docs.rs/rhizodb/latest/rhizodb/#upgrades)를 참고하세요.
Rust 1.100 이상이 필요합니다.

```bash
cargo add rhizodb
```

## 기능

* GQL 질의: `MATCH`, `OPTIONAL MATCH`, `INSERT`, `SET`, `REMOVE`, `DELETE`, `MERGE`, `FILTER`, `LET`, 가변 길이 경로, 레이블 식, GQL의 모든 간선 방향, 집계, `NULLS FIRST`와 `NULLS LAST`를 쓰는 정렬, 값 목록과 비교식을 받는 단순 `CASE` 분기를 지원합니다.
* 하위 질의와 질의 조합: `EXISTS`, `COUNT`, `COLLECT` 하위 질의, `CALL { ... }` 하위 질의, `UNION`, `EXCEPT`, `INTERSECT`, `OTHERWISE`를 지원합니다.
* 값: `||` 연결과 정규식 매칭을 지원하는 문자열, 리스트, 바이트 문자열, 날짜, 시각, 시간대가 있는 날짜시각, 기간을 다룹니다.
* ACID 트랜잭션: 쓰기 트랜잭션은 한 번에 하나만 열리고, 스냅숏 읽기와 쓰기는 서로를 막지 않습니다. `begin_read`, `begin_write`, 또는 단발성 `execute`로 사용합니다.
* 기본 설정에서 비정상 종료에 안전합니다. 커밋은 디스크 동기화가 끝난 뒤에 반환됩니다. 테스트는 저장소 작업을 하나씩 실패시키고, 전원을 끊고, 쓰는 중인 프로세스를 죽인 다음, 다시 연 파일을 검사합니다.
* 손상된 데이터를 그대로 내주지 않습니다. 모든 페이지에 체크섬이 있어서, 손상된 파일은 `ErrorKind::CorruptData`로 실패합니다.
* 속성 인덱스, 구문 검색과 접두어 검색을 지원하는 BM25 키워드 인덱스, 전수 비교(exact) 벡터 인덱스가 있고, 모두 데이터와 같은 트랜잭션 안에서 갱신됩니다.
* 온라인 백업, 그리고 버전이 붙은 JSON Lines 내보내기와 가져오기를 지원합니다.
* 프로그램이 직접 연결하는 내장 복제: writer 하나와, 그 변경을 적용하고 다른 복제본으로 넘겨주는 복제본으로 이뤄집니다. 텀(term) 번호로 예전 writer를 막고, 쓰기를 하나도 잃지 않고 writer 역할을 넘기고, 쓰기가 계속되는 동안 스냅숏으로 새 복제본을 초기화합니다.
* 프로그램이 등록한 프로시저를 질의에서 `CALL`로 실행합니다.
* `unsafe` 코드가 없습니다.

## 벤치마크

C++로 만든 내장 그래프 데이터베이스 [LadybugDB](https://ladybugdb.com/)와 비교했습니다. 생성한 소셜 그래프 하나로 비교했고, 두 엔진 모두 파일에 저장하며 기본 내구성 설정을 그대로 썼습니다.
매번 두 엔진이 같은 행을 돌려주는지 확인하고, 대량 적재는 엔진마다 가장 빠른 방법을 씁니다.
벤치마크 코드와 모든 질의는 [`crates/bench`](https://github.com/mathbook3948/rhizodb/tree/main/crates/bench)에 있습니다. `mise run bench-readme`를 실행하면 다시 재서 이 표를 새로 씁니다.

<!-- bench-table:start -->
2026-11-28에 4 CPUs (Intel(R) Xeon(R) Processor), Linux 6.18.44-fc-v880에서 쟀습니다. 규모는 `medium`(사람 20000명, `KNOWS` 관계 200000개, 도시 100개)이고, 작업마다 5번씩 반복했습니다. 시간은 작업 전체에 걸린 시간의 중앙값이고 단위는 밀리초입니다. 낮을수록 빠르고, 반복해 잰 값의 범위가 두 엔진에서 겹치면 '유사'로 표시합니다.

| 작업 | RhizoDB | LadybugDB | 더 빠른 쪽 |
| --- | ---: | ---: | --- |
| id로 노드 찾기 | 0.416 | 0.847 | RhizoDB 2.0배 |
| 조건으로 걸러 읽기 | 0.303 | 0.378 | RhizoDB 1.2배 |
| `ORDER BY ... LIMIT` | 0.497 | 0.841 | RhizoDB 1.7배 |
| 1홉 이웃 | 0.316 | 0.608 | RhizoDB 1.9배 |
| 1홉 이웃과 관계 속성 | 0.484 | 0.618 | RhizoDB 1.3배 |
| 관계 속성 합계 | 52.1 | 1.40 | LadybugDB 37.1배 |
| 2홉 이웃(중복 제거) | 0.644 | 8.57 | RhizoDB 13.3배 |
| 3홉 이웃(중복 제거) | 4.61 | 32.7 | RhizoDB 7.1배 |
| 가변 길이 `*1..3` | 5.08 | 26.8 | RhizoDB 5.3배 |
| 도시별 집계 | 1.88 | 2.49 | RhizoDB 1.3배 |
| 텍스트 속성에서 부분 문자열 찾기 | 20.8 | 29.3 | RhizoDB 1.4배 |
| 키워드 검색 | 52.1 | 375 | RhizoDB 7.2배 |
| 벡터 검색 | 203 | 398 | RhizoDB 2.0배 |
| 작은 커밋 | 67.0 | 203 | RhizoDB 3.0배 |
| 대량 수정 | 12.4 | 12.9 | 유사 |
| `DETACH DELETE` | 242 | 375 | RhizoDB 1.5배 |
| 대량 적재 | 676 | 905 | RhizoDB 1.3배 |
| 대량 적재 후 파일 크기 | 12.9 MiB | 13.0 MiB | 유사 |
<!-- bench-table:end -->

표는 실행한 컴퓨터에서 매번 새로 재기 때문에, 나중 표와 예전 표는 다른 컴퓨터에서 쟀을 수 있습니다.
한 줄은 id로 찾기 100번이나 작은 커밋 500번처럼 작업 묶음 전체에 걸린 시간입니다. 그러니 시간을 질의 하나의 지연 시간으로 읽지 말고, 같은 줄 안에서 두 엔진을 비교하세요.
RhizoDB의 벡터 인덱스는 전수 비교 방식이고 LadybugDB의 벡터 인덱스는 HNSW 그래프입니다. 이번 측정에서는 둘 다 실제 최근접 이웃을 모두 찾았습니다.
파일 크기와 대량 수정에서는 두 엔진이 비슷합니다.
모든 관계의 속성을 더하는 작업은 LadybugDB가 훨씬 빠릅니다. RhizoDB는 관계마다 그 관계가 든 청크를 따로 읽기 때문입니다.

## 개발

RhizoDB는 [mise](https://mise.jdx.dev)로 Rust 툴체인과 검사 도구의 버전을 고정합니다.

```bash
mise install
mise run check
```

`mise run check`는 포맷, 린트, 결정적 테스트, 예제, 저장소 규칙 검사를 돌립니다. CI도 모든 풀 리퀘스트에서 Linux, macOS, Windows로 같은 검사를 돌립니다.
테스트에는 openCypher TCK와 직접 작성한 GQL 시나리오가 들어 있습니다.
`mise run random`은 `mise run check`에서 빠지는 무작위 테스트, 비정상 종료 테스트, 부하 테스트를 돌립니다. CI는 `main`에 푸시할 때마다 이것을 돌립니다.
`mise run differential`은 질의 결과를 LadybugDB와 비교합니다. 야간 워크플로는 여기에 `mise run storage-deep`, `mise run random-deep`, `mise run soak`, 퍼징을 더 돌립니다.
설계 결정은 [`docs/`](https://github.com/mathbook3948/rhizodb/blob/main/docs/README.md)에 있습니다.
기여하려면 [CONTRIBUTING](https://github.com/mathbook3948/rhizodb/blob/main/.github/CONTRIBUTING.md)을 읽어 주세요. 보안 문제는 [보안 정책](https://github.com/mathbook3948/rhizodb/blob/main/.github/SECURITY.md)에 따라 알려 주세요.

## 라이선스

다음 둘 중 하나를 골라 그 조건으로 쓸 수 있습니다.

 * Apache License, Version 2.0
   ([LICENSE-APACHE](https://github.com/mathbook3948/rhizodb/blob/main/LICENSE-APACHE) 또는 http://www.apache.org/licenses/LICENSE-2.0)
 * MIT license
   ([LICENSE-MIT](https://github.com/mathbook3948/rhizodb/blob/main/LICENSE-MIT) 또는 http://opensource.org/licenses/MIT)

### 기여

따로 밝히지 않는 한, 여러분이 이 프로젝트에 넣으려고 일부러 제출한 기여는 Apache-2.0 라이선스의 정의에 따라 위와 같이 두 라이선스로 제공되며, 다른 조건은 붙지 않습니다.
