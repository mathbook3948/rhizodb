# RhizoDB

An embedded, schemaless property graph database for Rust, queried with GQL.

Status: early development. The API and the on-disk format will change without notice.

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

`mise run check` runs formatting, lints, tests, and the repository guards. The tests include the openCypher TCK, run against the engine with a checked-in list of scenarios that must pass (`crates/tck/passing.txt`), and a crash test that kills a writing process and checks the reopened store. To record newly passing TCK scenarios, run `TCK_BLESS=1 cargo test -p tck --test tck`. To fuzz the parser, run `cargo +nightly fuzz run parse`.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this project by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
