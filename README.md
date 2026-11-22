# RhizoDB

An embedded, schemaless property graph database for Rust, queried with GQL.

Status: early development, below 1.0. A breaking change to the API or the on-disk format raises the minor version and is listed in [CHANGELOG.md](https://github.com/mathbook3948/rhizodb/blob/main/CHANGELOG.md). A database written by an older version is upgraded when it is opened.

```rust
use rhizodb::Database;
use rhizodb::Parameters;

let db = Database::open("graph.redb")?;
db.execute(
    "INSERT (:Person {name: 'Ada'})-[:KNOWS]->(:Person {name: 'Grace'})",
    &Parameters::new(),
)?;
let result = db.execute(
    "MATCH (a:Person)-[:KNOWS]->(b) RETURN a.name, b.name",
    &Parameters::new(),
)?;
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

## Testing

`mise run check` runs formatting, lints, tests, and the repository guards. The tests include the openCypher TCK, run against the engine with a checked-in list of scenarios that must pass (`crates/tck/passing.txt`), and a crash test that kills a writing process and checks the reopened store. To record newly passing TCK scenarios, run `TCK_BLESS=1 cargo test -p tck --test tck`. GQL syntax outside the Cypher overlap is checked by hand-written scenarios in the same format under `crates/tck/gql/features`, one directory per ISO/IEC 39075 clause, with their own list in `crates/tck/gql/passing.txt`. Record those with `TCK_BLESS=1 cargo test -p tck --test gql`. To fuzz the parser, run `cargo +nightly fuzz run parse`.

`mise run differential` runs the same queries on RhizoDB and [LadybugDB](https://ladybugdb.com/) and compares the results. It builds LadybugDB from C++ source, which needs CMake, a C++ compiler, and OpenSSL, so it stays out of `mise run check` and runs in its own CI workflow. The queries come from `crates/differential/corpus.txt`, a seeded generator, and the graph-free TCK scenarios. `crates/differential/differences.txt` lists every query on which the engines do not agree. To rewrite it after a change, run `DIFFERENTIAL_BLESS=1 cargo test` in `crates/differential`, then replace each `UNEXPLAINED` note with the reason the engines differ.

## License

Licensed under either of [Apache License, Version 2.0](https://github.com/mathbook3948/rhizodb/blob/main/LICENSE-APACHE) or [MIT license](https://github.com/mathbook3948/rhizodb/blob/main/LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this project by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
