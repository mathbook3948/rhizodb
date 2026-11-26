# Changelog

All notable changes to `rhizodb` are recorded here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html). A released section's heading is exactly `## <version>`, such as `## 0.1.0`, because the release workflow reads it as the release notes.

## Unreleased

### Changed

- `min` and `max` keep a running result per group instead of every value, so `MATCH (p:Person) RETURN max(p.name), min(p.age)` over twenty thousand persons runs five times as fast and holds one value per group.
- A query reads each label, relationship type, and property key name from storage once, so returning nodes and relationships, `type(r)`, and `keys(n)` no longer read storage per element. `keys(n)` over twenty thousand nodes runs twice as fast.
- A pattern whose start node has no ID lookup or index starts from its end node when that node is already bound or has one, so `MATCH (q:Person)-[:LIVES_IN]->(c)` after `c` is bound, or `MATCH (:Person)-[k:KNOWS]->(h:Person {id: $id})`, reads only the relationships of that node. Grouping the oldest person of each of a hundred cities this way runs 33 times as fast on the medium benchmark graph.
- A `MATCH` filter whose every condition is a comparison, `IS NULL`, `IN` a list, or a string test on the elements its pattern binds tests each condition as soon as the elements it reads are bound, so a traversal with a filter on every hop stops a partial match at the first hop it fails. A three-hop traversal with a filter on every hop runs twice as fast.
- A query that reads the properties of many relationships reads each stored chunk of 32 relationships from storage once rather than once per relationship, so counting the `KNOWS` relationships of the medium benchmark graph with a filter on a relationship property runs four times as fast.
- A query that reaches a node again over the same kind of relationship, as one that closes a cycle does, reads that node's adjacency from storage at most twice, so counting every `KNOWS` triangle of the medium benchmark graph runs three times as fast.
- A scan of one label computes operators, function calls, and indexing over the properties it reads, so a query that orders, groups, or filters by such a value, such as `ORDER BY p.age * 7 % 13 + p.id % 5 LIMIT 20` or `RETURN upper(substring(p.name, 0, 1)), count(*)`, no longer builds a row per node. Ordering twenty thousand persons by a computed score runs six times as fast.
- `EXISTS { MATCH ... }` and `COUNT { MATCH ... }` over a single `MATCH` count the matches without building a row for each, and `EXISTS` stops at the first match when nothing it would test after that could fail. Filtering the persons of the medium benchmark graph by `COUNT { MATCH (p)<-[:KNOWS]-(:Person) } >= 20` runs twice as fast.

## 0.1.0

The first release: an embedded, schemaless property graph database for Rust programs, queried with GQL.

### Added

#### Storage and durability

- One database file of RhizoDB's own storage engine, beside a write-ahead log that a clean close empties. Every page carries a checksum, so damage fails with `ErrorKind::CorruptData` where it is read.
- A commit returns once its log record is synced, and a crash or power loss keeps every acknowledged commit. Checkpoints write the log into the file in the background.
- An exclusive lock on the open file, on a file system with file locks, so a second `Database` on the same file, in this process or another, fails with `ErrorKind::DatabaseAlreadyOpen`.
- Nodes keep their labels as interned label sets and their properties in typed columns per property key, and relationships live in compressed adjacency segments, so scans, filters, aggregates, and label counts read only what they use.
- Automatic space reclaim: a file that holds much more than its live data is compacted after a checkpoint, so a long-running host's file does not grow without bound.
- `Database::in_memory` and `OpenOptions::in_memory` run the same engine in memory.

#### Query language

- GQL queries through `Database::execute`, with `MATCH`, `OPTIONAL MATCH`, `INSERT` (and Cypher `CREATE`), `SET`, `REMOVE`, `DELETE`, `DETACH DELETE`, `MERGE`, `UNWIND`, `WITH`, `FILTER`, `LET`, `NEXT`, `ORDER BY ... NULLS FIRST | LAST`, `UNION`, `EXCEPT`, `INTERSECT`, and `OTHERWISE`.
- Label expressions with `|`, `&`, `!`, `%`, and parentheses, every GQL edge orientation, `IS [NOT] LABELED`, and variable-length relationships in the Cypher form `-[:R*1..3]->`. GQL path quantifiers such as `->{1,3}` are not supported and fail as a syntax error.
- Subqueries: `EXISTS`, `COUNT`, `COLLECT`, and `VALUE { ... }`, and `CALL { ... }` with variable scope clauses, `OPTIONAL CALL`, and updating bodies.
- Values: null, booleans, integers, floats, strings, bytes, lists, maps, nodes, relationships, paths, and temporals (dates, times, datetimes with zones, and durations).
- Type predicates `IS [NOT] TYPED` and `IS [NOT] ::`, `CAST(value AS type)`, `valueType()`, `normalize()` and `IS [NOT] NORMALIZED`, `IS [NOT] TRUE | FALSE | UNKNOWN`, and `IS [NOT] DISTINCT FROM`.
- The GQL string, numeric, list, and temporal functions, regular expressions with `=~`, `reduce()`, and `vector.similarity.cosine` and `vector.similarity.euclidean`.
- Host-registered procedures with `Database::register_procedure`, called with `CALL` outside the `db` namespace that built-in procedures reserve.

#### Transactions

- Every query runs in its own transaction, and `Database::begin_read` and `Database::begin_write` open explicit ones: snapshot reads, and one writer at a time.
- Read queries that match from many start nodes split them across threads, set with `OpenOptions::threads` and `OpenOptions::parallel_threshold`, and return the same rows in the same order as on one thread.

#### Indexes

- Keyword (BM25) indexes with `"quoted phrases"` and `prefix*` terms and text compared in Unicode normalization form NFKC. Text without spaces between words, such as Hangul, Han, Hiragana, Katakana, Thai, Lao, Khmer, and Myanmar, is indexed as overlapping pairs of characters, since no word dictionary splits it.
- Exact vector indexes with cosine, euclidean, and dot product metrics.
- Property indexes over one property, which `MATCH` and `MERGE` read for `=`, `IN`, `<`, `<=`, `>`, and `>=`.
- Indexes are created and dropped through `Database::create_keyword_index`, `Database::create_vector_index`, `Database::create_property_index`, and `Database::drop_index`, which take an `IndexTarget` and, for vectors, a `VectorMetric`. GQL has no index statements.
- Every index covers the nodes of a label or the relationships of a type, changes in the same transaction as the data it covers, and is searched with `CALL db.index.keyword.queryNodes`, `queryRelationships`, and their `db.index.vector` counterparts.

#### Operations

- Online backups with `Database::backup_to`, which copy one snapshot into a new file while readers and writers carry on and never leave a partial file. Restoring a backup is opening it.
- Logical dumps with `Database::export` and `Database::import`, a versioned JSON Lines format that keeps IDs, labels, index definitions, and every value's exact type.
- `Database::compact`, `Database::stats`, and `Database::check` for giving space back, reading the file's size, live data, log, and cache counts, and checking every page.

#### API

- `Database`, `OpenOptions`, `DatabaseStats`, `ReadTransaction`, `WriteTransaction`, `QueryResult`, `Value`, `Node`, `Relationship`, `Path`, `NodeId`, `RelationshipId`, `Parameters`, `IndexTarget`, `VectorMetric`, `Procedure`, `ProcedureOutput`, `ProcedureError`, `ValueType`, and the temporal types.
- `Error` with `Error::kind` for a stable `ErrorKind`, and `Error::byte_offset` and `SyntaxErrorKind` for where and why the parser refused a query.
- `Display` for `Value`, `Node`, `Relationship`, and `Path` as GQL literals, so a host can print query results without writing a formatter.
- `Value`, `Temporal`, `ErrorKind`, `SyntaxErrorKind`, `ValueType`, and `DatabaseStats` are `#[non_exhaustive]`, so later releases can add to them, and `WriteTransaction` stays on the thread that began it.
- The minimum supported Rust version is 1.100.

### Notes

- A database file written by a pre-release build opens only if its graph format is 11 or later, and an older one fails with `ErrorKind::UnsupportedFormatVersion`. Files this release writes use engine format 1.3, which earlier pre-release builds refuse.
- An open that finds the file lock held tries again for about half a second before it fails with `ErrorKind::DatabaseAlreadyOpen`, since a child process the host spawns shares the lock until it starts its program.
- `OpenOptions::create(false)` refuses a missing or empty file with `ErrorKind::DatabaseNotFound`, and leaves an empty file as it was.
- `Database::import` makes write transactions on other threads wait until it ends.
- The first open after a change of the keyword tokenizer, such as a dependency update that brings new Unicode tables, rebuilds every keyword index in that open.

### Known limitations

- An open that reads a header only the operating system's page cache held, as after a failed fsync or a process that stopped before its header was synced, writes its clean header into the slot of the one durable header. A power loss that tears that write falls back to an older header and loses acknowledged commits without an error.
