# RhizoDB

[![CI](https://github.com/mathbook3948/rhizodb/actions/workflows/check.yml/badge.svg)](https://github.com/mathbook3948/rhizodb/actions/workflows/check.yml)
[![Crates.io](https://img.shields.io/crates/v/rhizodb.svg)](https://crates.io/crates/rhizodb)
[![Documentation](https://docs.rs/rhizodb/badge.svg)](https://docs.rs/rhizodb)
[![License](https://img.shields.io/crates/l/rhizodb)](#license)

An embedded, schemaless property graph database for Rust, queried with GQL.

RhizoDB runs inside your program, with no server: a database is one file and its write-ahead log, or memory.
It stores nodes with labels, relationships with a type, and properties on both, and answers ISO GQL queries.
Cypher's `CREATE` is accepted as GQL `INSERT`.
Node properties live in typed columns and relationships in compressed sparse row segments, so scans, aggregates, and multi-hop traversals read only the data they need.

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

`Database::open("graph.rhizodb")` keeps the same graph in a file instead.
More runnable programs are in [`crates/rhizodb/examples`](https://github.com/mathbook3948/rhizodb/tree/main/crates/rhizodb/examples): a file-backed social graph with transactions and traversals, keyword and vector search, and a host-registered procedure.
Run one from a clone with `cargo run --example social`.

## Status

RhizoDB is in early development, below 1.0, and is meant for Rust programs that want a graph store in-process, the way SQLite is a relational one.
A breaking change to the API or the on-disk format raises the minor version and is listed in [CHANGELOG.md](https://github.com/mathbook3948/rhizodb/blob/main/CHANGELOG.md).
A database written by an older version is upgraded in place when it is opened, and a file of an older build that stored its data with redb is converted to the current format.
The [upgrade guide](https://docs.rs/rhizodb/latest/rhizodb/#upgrades) says what that means for backups.
RhizoDB needs Rust 1.100 or later.

```bash
cargo add rhizodb
```

Until the first release is on crates.io, depend on the Git repository instead:

```toml
[dependencies]
rhizodb = { git = "https://github.com/mathbook3948/rhizodb" }
```

## Features

* GQL queries: `MATCH`, `OPTIONAL MATCH`, `INSERT`, `SET`, `REMOVE`, `DELETE`, `MERGE`, `FILTER`, `LET`, variable-length paths, label expressions, every GQL edge orientation, aggregation, ordering with `NULLS FIRST` and `NULLS LAST`, and simple `CASE` branches with lists of values and comparisons.
* Subqueries and query composition: `EXISTS`, `COUNT`, and `COLLECT` subqueries, `CALL { ... }` subqueries, `UNION`, `EXCEPT`, `INTERSECT`, and `OTHERWISE`.
* Values: strings with `||` concatenation and regular expression matching, lists, byte strings, dates, times, datetimes with zones, and durations.
* ACID transactions with one writer and snapshot readers that never block each other, through `begin_read`, `begin_write`, or a one-shot `execute`.
* Crash safe by default: a commit returns once it is synced, and tests fail each storage operation in turn, cut power, and kill writing processes before checking the reopened file.
* Damage is detected, not served: every page carries a checksum, and a damaged file fails with `ErrorKind::CorruptData`.
* Property indexes, BM25 keyword indexes with phrases and prefixes, and exact vector indexes, all updated in the same transaction as the data they index.
* Online backups, and a versioned JSON Lines export and import.
* Procedures that the host program registers and queries run with `CALL`.
* No `unsafe` code.

## Benchmarks

RhizoDB against [LadybugDB](https://ladybugdb.com/), an embedded graph database written in C++, on one generated social graph, both file-backed with their default durability.
Every run checks that both engines return the same rows, and bulk load uses each engine's own fastest path.
The suite and every query are in [`crates/bench`](https://github.com/mathbook3948/rhizodb/tree/main/crates/bench), and `mise run bench-readme` measures again and rewrites this table.

<!-- bench-table:start -->
Measured on 2026-11-25 on 4 CPUs (Intel(R) Xeon(R) Processor @ 2.10GHz), Linux 6.18.44-fc-v880, at the `medium` scale (20000 persons, 200000 `KNOWS` edges, 100 cities), with 5 repetitions per workload. Times are medians in milliseconds for the whole workload, lower is better, and a row whose ranges of runs overlap is within noise.

| Workload | RhizoDB | LadybugDB | Faster |
| --- | ---: | ---: | --- |
| Point lookup by id | 0.405 | 0.989 | RhizoDB 2.4x |
| Filtered scan | 1.07 | 0.748 | LadybugDB 1.4x |
| `ORDER BY ... LIMIT` | 2.22 | 1.33 | LadybugDB 1.7x |
| 1-hop neighbours | 0.586 | 1.19 | RhizoDB 2.0x |
| 2-hop distinct | 0.821 | 14.9 | RhizoDB 18.2x |
| 3-hop distinct | 5.82 | 51.7 | RhizoDB 8.9x |
| Variable length `*1..3` | 5.59 | 42.6 | RhizoDB 7.6x |
| Group by city | 5.90 | 3.86 | LadybugDB 1.5x |
| Substring scan of a text property | 49.3 | 37.3 | LadybugDB 1.3x |
| Keyword search | 103 | 752 | RhizoDB 7.3x |
| Vector search | 320 | 512 | RhizoDB 1.6x |
| Small commits | 115 | 284 | RhizoDB 2.5x |
| Bulk update | 16.5 | 16.1 | within noise |
| Detach delete | 330 | 465 | RhizoDB 1.4x |
| Bulk load | 1535 | 1070 | LadybugDB 1.4x |
| File size after bulk load | 20.0 MiB | 13.0 MiB | LadybugDB 1.5x smaller |
<!-- bench-table:end -->

A row times a whole batch of operations, such as 100 point lookups or 500 small commits, so compare the engines within a row rather than reading a time as one query's latency.
RhizoDB's vector index is exact and LadybugDB's is an HNSW graph, and both found every true nearest neighbour here.
LadybugDB is faster at bulk load, scans, sorting, and grouping, and keeps a smaller file.

## Development

RhizoDB uses [mise](https://mise.jdx.dev) to pin the Rust toolchain and the check tools.

```bash
mise install
mise run check
```

`mise run check` runs formatting, lints, the deterministic tests, the examples, and the repository guards, as CI does on Linux, macOS, and Windows for every pull request.
The tests include the openCypher TCK and hand-written GQL scenarios.
`mise run random` runs the randomized, crash, and stress tests that `mise run check` leaves out, as CI does for every push to `main`.
`mise run differential` compares query results with LadybugDB, and the nightly workflow adds `mise run storage-deep`, `mise run random-deep`, `mise run soak`, and fuzzing.
The design decisions are in [`docs/`](https://github.com/mathbook3948/rhizodb/blob/main/docs/README.md).
To contribute, read [CONTRIBUTING](https://github.com/mathbook3948/rhizodb/blob/main/.github/CONTRIBUTING.md), and to report a security problem, follow the [security policy](https://github.com/mathbook3948/rhizodb/blob/main/.github/SECURITY.md).

## License

Licensed under either of

 * Apache License, Version 2.0
   ([LICENSE-APACHE](https://github.com/mathbook3948/rhizodb/blob/main/LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
 * MIT license
   ([LICENSE-MIT](https://github.com/mathbook3948/rhizodb/blob/main/LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
