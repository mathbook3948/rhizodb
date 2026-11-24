# RhizoDB

[![CI](https://github.com/mathbook3948/rhizodb/actions/workflows/check.yml/badge.svg)](https://github.com/mathbook3948/rhizodb/actions/workflows/check.yml)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![Rust edition](https://img.shields.io/badge/rust%20edition-2024-orange.svg)](https://doc.rust-lang.org/edition-guide/rust-2024/index.html)

An embedded, schemaless property graph database for Rust, queried with GQL.

RhizoDB runs inside your program, with no separate server. It keeps nodes, relationships, and their properties in one file of its own storage engine, with a write-ahead log beside it, answers GQL queries, and updates its keyword and vector indexes in the same transaction as the data they index.

```rust
use rhizodb::Database;
use rhizodb::Parameters;

let db = Database::open("graph.rhizodb")?;
db.execute(
    "INSERT (:Person {name: 'Ada'})-[:KNOWS]->(:Person {name: 'Grace'})",
    &Parameters::new(),
)?;
let result = db.execute(
    "MATCH (a:Person)-[:KNOWS]->(b) RETURN a.name, b.name",
    &Parameters::new(),
)?;
```

## Status

RhizoDB is in early development, below 1.0, and is not on crates.io yet. A breaking change to the API or the on-disk format raises the minor version and is listed in [CHANGELOG.md](https://github.com/mathbook3948/rhizodb/blob/main/CHANGELOG.md). A database written by an older version is upgraded when it is opened.

## Features

* Embedded. The database is one file, and a write-ahead log beside it, that your program opens, with no server to run.
* Its own storage engine, which checksums every page, checkpoints in the background, and gives back free space by compacting the file. A file of an earlier build, stored with redb, is converted when it is opened.
* Schemaless property graph. Nodes carry labels, relationships carry a type, and both carry any properties.
* GQL queries. The part that overlaps with Cypher is checked against the openCypher TCK, and Cypher `CREATE` is accepted as GQL `INSERT`.
* GQL pattern syntax, including label expressions, every edge orientation, `FILTER`, `LET`, and `EXISTS { ... }` subqueries.
* Dates, times, datetimes with zones, durations, and regular expression matching with `=~`.
* Read snapshots and write transactions that commit or roll back as a whole.
* Keyword (BM25) and exact vector indexes over node or relationship properties.
* Procedures that the host program registers and queries run with `CALL`.
* A versioned on-disk format, tested by killing a writing process and checking the reopened store.

## Installation and usage

Until the first release is on crates.io, depend on the Git repository. RhizoDB needs Rust 1.100 or later.

```toml
[dependencies]
rhizodb = { git = "https://github.com/mathbook3948/rhizodb" }
```

Each `execute` call is its own transaction. To make several queries take effect together, run them in a write transaction. It commits only when asked, and rolls back when dropped or when one of its queries fails.

```rust
let mut tx = db.begin_write()?;
tx.execute("INSERT (:Person {name: 'Alan'})", &Parameters::new())?;
tx.execute(
    "MATCH (a:Person {name: 'Alan'}), (b:Person {name: 'Ada'}) INSERT (a)-[:KNOWS]->(b)",
    &Parameters::new(),
)?;
tx.commit()?;
```

`db.begin_read()` gives a snapshot that several queries read, unchanged by commits made after it began. One write transaction is open at a time, and `begin_write` waits for the open one to end. A writing query or `begin_write` on the thread that already holds the open write transaction fails instead of waiting for itself.

## Development

RhizoDB uses [mise](https://mise.jdx.dev) to pin the Rust toolchain and the check tools. To run every check that CI runs:

```bash
mise install
mise run check
```

`mise run check` runs formatting, lints, tests, and the repository guards. The tests include the openCypher TCK, run against the engine with a checked-in list of scenarios that must pass (`crates/tck/passing.txt`), and a crash test that kills a writing process and checks the reopened store. To record newly passing TCK scenarios, run `TCK_BLESS=1 cargo test -p tck --test tck`. GQL syntax outside the Cypher overlap is checked by hand-written scenarios in the same format under `crates/tck/gql/features`, one directory per ISO/IEC 39075 clause, with their own list in `crates/tck/gql/passing.txt`. Record those with `TCK_BLESS=1 cargo test -p tck --test gql`. To fuzz the parser, run `cargo +nightly fuzz run parse`.

`mise run differential` runs the same queries on RhizoDB and [LadybugDB](https://ladybugdb.com/) and compares the results. It builds LadybugDB from C++ source, which needs CMake, a C++ compiler, and OpenSSL, so it stays out of `mise run check` and runs in its own CI workflow. The queries come from `crates/differential/corpus.txt`, a seeded generator, and the graph-free TCK scenarios. `crates/differential/differences.txt` lists every query on which the engines do not agree. To rewrite it after a change, run `DIFFERENTIAL_BLESS=1 cargo test` in `crates/differential`, then replace each `UNEXPLAINED` note with the reason the engines differ.

The design decisions are in [`docs/`](https://github.com/mathbook3948/rhizodb/blob/main/docs/README.md). To contribute, read [CONTRIBUTING](https://github.com/mathbook3948/rhizodb/blob/main/.github/CONTRIBUTING.md). To report a security problem, follow the [security policy](https://github.com/mathbook3948/rhizodb/blob/main/.github/SECURITY.md).

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
