# Changelog

All notable changes to `rhizodb` are recorded here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html). A released section's heading is exactly `## <version>`, such as `## 0.1.0`, because the release workflow reads it as the release notes.

## Unreleased

## 0.1.0

The first release: an embedded, schemaless property graph database for Rust programs, queried with GQL.

### Added

#### Storage and durability

- One database file of RhizoDB's own storage engine, beside a write-ahead log that a clean close empties. Every page carries a checksum, so damage fails with `ErrorKind::CorruptData` where it is read.
- A commit returns once its log record is synced, and a crash or power loss keeps every acknowledged commit. Checkpoints write the log into the file in the background.
- An exclusive lock on the open file, so a second `Database` on the same file, in this process or another, fails with `ErrorKind::DatabaseAlreadyOpen`.
- Nodes keep their labels as interned label sets and their properties in typed columns per property key, and relationships live in compressed adjacency segments, so scans, filters, aggregates, and label counts read only what they use.
- Automatic space reclaim: a file that holds much more than its live data is compacted after a checkpoint, so a long-running host's file does not grow without bound.
- `Database::in_memory` and `OpenOptions::in_memory` run the same engine in memory.

#### Query language

- GQL queries through `Database::execute`, with `MATCH`, `OPTIONAL MATCH`, `INSERT` (and Cypher `CREATE`), `SET`, `REMOVE`, `DELETE`, `DETACH DELETE`, `MERGE`, `UNWIND`, `WITH`, `FILTER`, `LET`, `NEXT`, `ORDER BY ... NULLS FIRST | LAST`, `UNION`, `EXCEPT`, `INTERSECT`, and `OTHERWISE`.
- Label expressions with `|`, `&`, `!`, `%`, and parentheses, every GQL edge orientation, variable-length paths, and `IS [NOT] LABELED`.
- Subqueries: `EXISTS`, `COUNT`, `COLLECT`, and `VALUE { ... }`, and `CALL { ... }` with variable scope clauses, `OPTIONAL CALL`, and updating bodies.
- Values: null, booleans, integers, floats, strings, lists, maps, nodes, relationships, paths, and temporals (dates, times, datetimes with zones, and durations).
- Type predicates `IS [NOT] TYPED` and `IS [NOT] ::`, `CAST(value AS type)`, `valueType()`, `normalize()` and `IS [NOT] NORMALIZED`, `IS [NOT] TRUE | FALSE | UNKNOWN`, and `IS [NOT] DISTINCT FROM`.
- The GQL string, numeric, list, and temporal functions, regular expressions with `=~`, `reduce()`, and `vector.similarity.cosine` and `vector.similarity.euclidean`.
- Host-registered procedures with `Database::register_procedure`, called with `CALL` outside the `db` namespace that built-in procedures reserve.

#### Transactions

- Every query runs in its own transaction, and `Database::begin_read` and `Database::begin_write` open explicit ones: snapshot reads, and one writer at a time.
- Read queries that match from many start nodes split them across threads, set with `OpenOptions::threads` and `OpenOptions::parallel_threshold`, and return the same rows in the same order as on one thread.

#### Indexes

- Keyword (BM25) indexes with `"quoted phrases"` and `prefix*` terms, text compared in Unicode normalization form NFKC, and word splitting that handles Thai, Lao, Khmer, Myanmar, and CJK text.
- Exact vector indexes with cosine, euclidean, and dot product metrics.
- Property indexes over one property, which `MATCH` and `MERGE` read for `=`, `IN`, `<`, `<=`, `>`, and `>=`.
- Every index covers the nodes of a label or the relationships of a type, changes in the same transaction as the data it covers, and is searched with `CALL db.index.keyword.queryNodes`, `queryRelationships`, and their `db.index.vector` counterparts.

#### Operations

- Online backups with `Database::backup_to`, which copy one snapshot into a new file while readers and writers carry on and never leave a partial file. Restoring a backup is opening it.
- Logical dumps with `Database::export` and `Database::import`, a versioned JSON Lines format that keeps IDs, labels, index definitions, and every value's exact type.
- `Database::compact`, `Database::stats`, and `Database::check` for giving space back, reading the file's size, live data, log, and cache counts, and checking every page.

#### API

- `Database`, `OpenOptions`, `ReadTransaction`, `WriteTransaction`, `QueryResult`, `Value`, `Node`, `Relationship`, `Path`, `Parameters`, `Procedure`, and the temporal types.
- `Error` with `Error::kind` for a stable `ErrorKind`, and `Error::byte_offset` for the place in the query text where a syntax error was noticed.
- `Display` for `Value`, `Node`, `Relationship`, and `Path` as GQL literals, so a host can print query results without writing a formatter.
- `Value` and `Temporal` are `#[non_exhaustive]`, and `WriteTransaction` stays on the thread that began it.

### Notes

- A database file written by a pre-release build opens only if its graph format is 11 or later, and an older one fails with `ErrorKind::UnsupportedFormatVersion`. Files this release writes use engine format 1.3, which pre-release builds refuse.
- An open that finds the file lock held tries again for about half a second before it fails with `ErrorKind::DatabaseAlreadyOpen`, since a child process the host spawns shares the lock until it starts its program.
- `OpenOptions::create(false)` refuses a missing or empty file with `ErrorKind::DatabaseNotFound`, and leaves an empty file as it was.
- `Database::import` makes write transactions on other threads wait until it ends.
- The first open after a change of the keyword tokenizer, such as a dependency update that brings new Unicode tables, rebuilds every keyword index in that open.

### Known limitations

- If an fsync fails and the process reopens the database, a header that only the operating system's cache held is rewritten in place, and a power loss that tears that write can fall back to an older header and lose acknowledged commits without an error.
