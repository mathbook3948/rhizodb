# RhizoDB

[![CI](https://github.com/mathbook3948/rhizodb/actions/workflows/check.yml/badge.svg)](https://github.com/mathbook3948/rhizodb/actions/workflows/check.yml)
[![Crates.io](https://img.shields.io/crates/v/rhizodb.svg)](https://crates.io/crates/rhizodb)
[![Documentation](https://docs.rs/rhizodb/badge.svg)](https://docs.rs/rhizodb)
[![License](https://img.shields.io/crates/l/rhizodb)](#라이선스)

[English](README.md) | 한국어

Rust 애플리케이션에 내장해서 쓰는 스키마리스 속성 그래프 데이터베이스입니다. 쿼리 언어로 GQL을 씁니다.

별도의 서버 없이 애플리케이션 프로세스 안에서 동작합니다. 데이터베이스는 파일 하나와 WAL(write-ahead log)로 구성되고, 메모리에만 띄워서 쓸 수도 있습니다.
레이블이 붙은 노드와 타입이 있는 관계를 저장하고, 노드와 관계 모두에 속성을 붙일 수 있으며, ISO GQL 쿼리를 실행합니다.
Cypher의 `CREATE`를 써도 GQL의 `INSERT`와 똑같이 동작합니다.
노드 속성은 타입별 컬럼에, 관계는 CSR(compressed sparse row) 세그먼트에 저장합니다. 덕분에 스캔, 집계, 멀티홉 탐색에서 필요한 데이터만 읽습니다.

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

`Database::in_memory()` 대신 `Database::open("graph.rhizodb")`로 열면 같은 그래프를 파일에 저장합니다.
[`crates/rhizodb/examples`](https://github.com/mathbook3948/rhizodb/tree/main/crates/rhizodb/examples)에는 바로 실행해 볼 수 있는 예제가 있습니다. 트랜잭션과 탐색을 보여 주는 파일 기반 소셜 그래프, 키워드 검색과 벡터 검색, 애플리케이션이 직접 등록하는 프로시저를 다룹니다.
저장소를 클론한 뒤 `cargo run --example social`처럼 실행하면 됩니다.

## 현재 상태

아직 1.0 이전의 초기 개발 단계입니다. SQLite로 관계형 데이터베이스를 애플리케이션에 내장하듯, 그래프 데이터베이스를 내장하고 싶은 Rust 애플리케이션을 위해 만들고 있습니다.
API나 디스크 포맷이 호환되지 않게 바뀌면 마이너 버전을 올리고 [CHANGELOG.md](https://github.com/mathbook3948/rhizodb/blob/main/CHANGELOG.md)에 기록합니다.
이전 버전에서 만든 데이터베이스는 열 때 해당 파일이 그 자리에서 새 포맷으로 업그레이드됩니다.
업그레이드가 백업에 어떤 영향을 주는지는 [업그레이드 안내](https://docs.rs/rhizodb/latest/rhizodb/#upgrades)를 참고하세요.
Rust 1.100 이상이 필요합니다.

```bash
cargo add rhizodb
```

## 기능

* GQL 쿼리: `MATCH`, `OPTIONAL MATCH`, `INSERT`, `SET`, `REMOVE`, `DELETE`, `MERGE`, `FILTER`, `LET`, 가변 길이 경로, 레이블 표현식, GQL의 모든 관계 방향, 집계, `NULLS FIRST`와 `NULLS LAST` 정렬, 값 목록과 비교식을 쓸 수 있는 단순 `CASE` 분기
* 서브쿼리와 쿼리 조합: `EXISTS`, `COUNT`, `COLLECT` 서브쿼리, `CALL { ... }` 서브쿼리, `UNION`, `EXCEPT`, `INTERSECT`, `OTHERWISE`
* 값 타입: 문자열(`||` 연결, 정규식 매칭), 리스트, 바이트 문자열, 날짜, 시간, 타임존이 포함된 날짜시간, 기간
* ACID 트랜잭션: 쓰기 트랜잭션은 한 번에 하나만 열리고, 스냅샷 읽기와 쓰기는 서로를 막지 않습니다. `begin_read`나 `begin_write`로 트랜잭션을 열거나, `execute`로 바로 실행할 수 있습니다.
* 크래시 안전성: 기본 설정에서 커밋은 디스크 동기화가 끝난 뒤에 반환됩니다. 스토리지 작업을 하나씩 실패시키거나, 전원을 끊거나, 쓰는 중인 프로세스를 강제로 종료한 다음 파일을 다시 열어 검사하는 테스트가 있습니다.
* 손상 감지: 모든 페이지에 체크섬이 있어서, 손상된 데이터를 그대로 돌려주지 않고 `ErrorKind::CorruptData` 오류를 반환합니다.
* 인덱스: 속성 인덱스, 구문 검색과 접두어 검색을 지원하는 BM25 키워드 인덱스, 전수 비교(exact) 벡터 인덱스를 제공하며, 모두 데이터와 같은 트랜잭션 안에서 갱신됩니다.
* 온라인 백업, 그리고 버전 정보가 포함된 JSON Lines 형식의 내보내기와 가져오기
* 임베디드 복제: 복제 구성은 애플리케이션이 직접 합니다. 쓰기 노드(writer) 하나가 쓰고, 복제본은 그 변경을 적용하면서 다른 복제본에 다시 전달할 수 있습니다. 텀(term) 번호로 이전 쓰기 노드를 차단하고, 쓰기 노드를 교체할 때도 쓰기를 하나도 잃지 않으며, 쓰기가 계속되는 중에도 스냅샷으로 새 복제본을 만들 수 있습니다.
* 프로시저: 애플리케이션이 등록한 프로시저를 쿼리에서 `CALL`로 호출할 수 있습니다.
* `unsafe` 코드가 없습니다.

## 벤치마크

C++로 작성된 임베디드 그래프 데이터베이스 [LadybugDB](https://ladybugdb.com/)와 비교했습니다. 자동 생성한 소셜 그래프 하나로 측정했고, 두 엔진 모두 파일에 저장하며 내구성 설정은 기본값을 그대로 둡니다.
매번 두 엔진이 같은 결과를 돌려주는지 확인하고, 대량 적재는 엔진마다 가장 빠른 방법을 씁니다.
벤치마크 코드와 쿼리는 전부 [`crates/bench`](https://github.com/mathbook3948/rhizodb/tree/main/crates/bench)에 있습니다. `mise run bench-readme`를 실행하면 다시 측정해서 이 표를 갱신합니다.

<!-- bench-table:start -->
2026-11-28에 16 CPUs (12th Gen Intel(R) Core(TM) i7-12700KF), Windows 10.0.26300.0 환경에서 측정했습니다. 규모는 `medium`(사람 20000명, `KNOWS` 관계 200000개, 도시 100개)이고 작업마다 5번 반복했습니다. 값은 작업 전체 시간의 중앙값(밀리초)이며 낮을수록 빠릅니다. 두 엔진의 반복 측정값 범위가 겹치면 '유사'로 표시합니다.

| 작업 | RhizoDB | LadybugDB | 더 빠른 쪽 |
| --- | ---: | ---: | --- |
| id로 노드 조회 | 0.430 | 1.00 | RhizoDB 2.3배 |
| 필터 스캔 | 0.943 | 0.479 | LadybugDB 2.0배 |
| `ORDER BY ... LIMIT` | 1.07 | 0.604 | LadybugDB 1.8배 |
| 1홉 이웃 | 0.309 | 0.660 | RhizoDB 2.1배 |
| 1홉 이웃과 관계 속성 읽기 | 0.388 | 0.693 | RhizoDB 1.8배 |
| 관계 속성 합계 | 18.2 | 1.42 | LadybugDB 12.9배 |
| 2홉 이웃(중복 제거) | 0.696 | 15.9 | RhizoDB 22.9배 |
| 3홉 이웃(중복 제거) | 4.44 | 52.4 | RhizoDB 11.8배 |
| 가변 길이 `*1..3` | 4.75 | 36.1 | RhizoDB 7.6배 |
| 도시별 집계 | 2.58 | 3.23 | RhizoDB 1.2배 |
| 텍스트 속성 부분 문자열 검색 | 25.2 | 42.9 | RhizoDB 1.7배 |
| 키워드 검색 | 42.4 | 298 | RhizoDB 7.0배 |
| 벡터 검색 | 169 | 357 | RhizoDB 2.1배 |
| 작은 커밋 | 268 | 412 | RhizoDB 1.5배 |
| 일괄 수정 | 21.2 | 27.6 | 유사 |
| `DETACH DELETE` | 225 | 499 | RhizoDB 2.2배 |
| 대량 적재 | 735 | 964 | RhizoDB 1.3배 |
| 대량 적재 후 파일 크기 | 12.9 MiB | 13.0 MiB | 유사 |
<!-- bench-table:end -->

이 표는 명령을 실행한 컴퓨터에서 매번 새로 측정하므로, 갱신할 때마다 측정 환경이 달라질 수 있습니다.
각 행은 id 조회 100번이나 작은 커밋 500번처럼 작업 묶음 전체에 걸린 시간입니다. 단일 쿼리의 지연 시간으로 읽지 말고, 같은 행에서 두 엔진을 비교하는 데 쓰세요.
RhizoDB의 벡터 인덱스는 전수 비교 방식이고, LadybugDB는 HNSW 그래프를 씁니다. 이번 측정에서는 둘 다 실제 최근접 이웃을 모두 찾았습니다.
파일 크기와 일괄 수정 시간은 두 엔진이 비슷합니다.
모든 관계의 속성을 합하는 작업은 LadybugDB가 훨씬 빠릅니다. RhizoDB는 관계 하나마다 그 관계가 속한 청크를 따로 읽기 때문입니다.
이 컴퓨터에서는 필터 스캔과 `ORDER BY ... LIMIT`도 LadybugDB가 더 빠릅니다.

## 개발

Rust 툴체인과 검사 도구의 버전은 [mise](https://mise.jdx.dev)로 고정합니다.

```bash
mise install
mise run check
```

`mise run check`는 포맷 검사, 린트, 결정적 테스트, 예제, 저장소 규칙 검사를 실행합니다. CI도 PR마다 Linux, macOS, Windows에서 같은 검사를 돌립니다.
테스트에는 openCypher TCK와 직접 작성한 GQL 시나리오가 들어 있습니다.
`mise run random`은 `mise run check`에 포함되지 않는 랜덤 테스트, 크래시 테스트, 부하 테스트를 실행합니다. CI는 `main`에 푸시될 때마다 이 테스트들을 돌립니다.
`mise run differential`은 쿼리 결과를 LadybugDB와 비교합니다. 매일 밤 도는 워크플로에서는 여기에 `mise run storage-deep`, `mise run random-deep`, `mise run soak`, 퍼징을 더 돌립니다.
설계 결정은 [`docs/`](https://github.com/mathbook3948/rhizodb/blob/main/docs/README.md)에 정리되어 있습니다.
기여하기 전에 [CONTRIBUTING](https://github.com/mathbook3948/rhizodb/blob/main/.github/CONTRIBUTING.md)을 읽어 주세요. 보안 문제는 [보안 정책](https://github.com/mathbook3948/rhizodb/blob/main/.github/SECURITY.md)에 따라 제보해 주세요.

## 라이선스

다음 두 라이선스 중 하나를 선택해서 사용할 수 있습니다.

 * Apache License, Version 2.0
   ([LICENSE-APACHE](https://github.com/mathbook3948/rhizodb/blob/main/LICENSE-APACHE) 또는 http://www.apache.org/licenses/LICENSE-2.0)
 * MIT license
   ([LICENSE-MIT](https://github.com/mathbook3948/rhizodb/blob/main/LICENSE-MIT) 또는 http://opensource.org/licenses/MIT)

### 기여

별도로 밝히지 않는 한, 이 프로젝트에 포함되도록 의도적으로 제출한 모든 기여(Apache-2.0 라이선스에서 정의하는 기여)는 다른 조건 없이 위의 두 라이선스로 이중 라이선스가 적용됩니다.
