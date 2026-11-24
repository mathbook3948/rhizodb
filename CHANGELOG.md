# Changelog

All notable changes to `rhizodb` are recorded here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html). A released section's heading is exactly `## <version>`, such as `## 0.1.0`, because the release workflow reads it as the release notes.

## Unreleased

### Added

- Embedded property graph storage on redb, with a versioned on-disk format that upgrades older databases on open.
- Node labels kept as interned label sets in one node meta chunk per 4096 node IDs, so label tests read no node record and `MATCH (n:Label) RETURN count(*)` reads one chunk per 4096 nodes.
- `count(n)` and counts of `MATCH (n)` without a label read the node meta counts as `MATCH (n:Label) RETURN count(*)` does, and a commit that changes a few nodes of a row group writes a small delta per node instead of the group's whole chunk.
- Node meta carries checksums, so a damaged chunk or delta fails the read as corrupt data instead of answering wrongly.
- Node properties kept in one column per property key and 4096 node IDs, typed when its values share a type, so a scan, filter, sort, or aggregate reads only the properties it uses, a commit that changes a few nodes writes a small checksummed delta per value, and a damaged column fails the read as corrupt data. Databases of earlier formats move their node records into columns on open.
- Relationships kept in compressed sparse row segments per 64 node IDs, relationship type, and direction, with a small checksummed delta per node for small commits, so a loaded graph takes about half the disk space and a damaged segment fails the read as corrupt data. Databases of earlier formats move their adjacency into segments on open.
- GQL queries through `Database::execute`, with Cypher `CREATE` accepted as GQL `INSERT`.
- Temporal values: dates, times, datetimes with zones, and durations.
- Host-registered procedures callable with `CALL`, outside the `db` namespace that built-in procedures reserve, and full `EXISTS { ... }` subqueries.
- Regular expression matching with `=~`.
- GQL linear statements `FILTER` and `LET`, label expressions with `|`, `&`, `!`, `%`, and parentheses in patterns and in `IS [NOT] LABELED`, label sets after `IS` or `&` in `SET` and `REMOVE`, and every GQL edge orientation, including `~`, `<~`, and `~>`.
- `COUNT { ... }` and `COLLECT { ... }` subqueries, and `reduce()`.
- `CALL { ... }` subqueries with GQL variable scope clauses, `OPTIONAL CALL`, and `UNION` inside, the GQL string and numeric functions, `trim(... FROM ...)`, simple `CASE` branches with lists of tests and comparisons, and `ORDER BY ... NULLS FIRST | LAST`.
- Explicit read and write transactions through `Database::begin_read` and `Database::begin_write`.
- Keyword (BM25) and exact vector indexes, kept in the same transaction as the data they index, created with `Database::create_keyword_index` and `Database::create_vector_index`, and searched with `CALL db.index.keyword.queryNodes` and `CALL db.index.vector.queryNodes`.
- Keyword and vector indexes over the properties of relationships of one type, created by passing `IndexTarget::RelationshipType` to `Database::create_keyword_index` and `Database::create_vector_index`, and searched with `CALL db.index.keyword.queryRelationships` and `CALL db.index.vector.queryRelationships`, which yield `relationship` and `score`.
- Keyword queries match `"quoted phrases"` and `prefix*` terms, compare text in Unicode normalization form NFKC, and find words inside Thai, Lao, Khmer, and Myanmar text.
- Property indexes over one property of the nodes of a label or the relationships of a type, created with `Database::create_property_index`, which `MATCH` and `MERGE` read for `=`, `IN`, `<`, `<=`, `>`, and `>=` on that property, and `id()` lookups in `WHERE` that read an element by ID instead of scanning.
- `OpenOptions`, whose `create(false)` fails with the new `ErrorKind::DatabaseNotFound` instead of creating a missing file, and where later settings will go.
- `NodeId::from_u64` and `RelationshipId::from_u64`, for a host that stored an ID as a number.
- `ErrorKind::DatabaseAlreadyOpen` and `ErrorKind::BackupTargetExists`, which opening a file that another `Database` holds and a backup to a path that already holds something report instead of `ErrorKind::Storage`.
- Online backups with `Database::backup_to`, which copies one snapshot of the committed database into a new file while readers and writers carry on, and never leaves a partial file at its path. Restoring a backup is opening it.
- Logical dumps with `Database::export` and `Database::import`: a versioned JSON Lines format that keeps node and relationship IDs, labels, index definitions, and every value's exact type, imported in bounded batches into a new database. New error kinds `Io`, `InvalidDump`, `UnsupportedDumpVersion`, and `DatabaseNotEmpty` report their failures.
- `Database::compact`, `Database::stats`, and `Database::check` for databases on RhizoDB's own storage engine: compaction gives back the space of deleted data, `stats` returns a `DatabaseStats` with the file's size and live bytes, its live and free extents, the log's size, and the cache's hits and misses, and `check` reads every page of the file and fails as corrupt data on the first broken invariant. On a database stored by redb they fail with `ErrorKind::Unsupported`.
- RhizoDB's own storage engine, still built only with `--cfg rhizodb_engine`, converts a redb database file when it opens one: it checks every redb page, copies every table into a new file, and renames that file over the redb file, which stays untouched until the copy is whole and synced. A damaged redb file fails as corrupt data and is left as it was.

### Changed

- `Value` and `Temporal` are `#[non_exhaustive]`, so a match on them needs a catch-all arm.
- `Node` and `Relationship` keep their fields private. Read them with `id()`, `labels()`, `has_label()`, `relationship_type()` (formerly the field `rel_type`), `start()`, `end()`, `properties()`, and `property()`.
- `Parameters` is a type of its own instead of an alias of `BTreeMap`. Build it with `new`, `insert`, `with`, `from` an array of pairs, or `collect`, and read it with `get`.
- A procedure body returns `Result<ProcedureOutput, ProcedureError>`, where `ProcedureOutput` is a type of its own built from a `Vec` of rows, by `collect`, or with `push_row`, and `ProcedureError` is `Box<dyn Error + Send + Sync>`.
- `Database::register_procedure` takes `&self`, so a database shared through an `Arc` can register procedures.
- The fuzzing entry points `read_value_for_fuzzing`, `decode_engine_formats_for_fuzzing`, `open_damaged_engine_file_for_fuzzing`, and `parse_for_fuzzing` exist only with the non-default `fuzzing` cargo feature, and `parse_for_fuzzing` moved from the hidden `gql` module to the crate root, which no longer exports `gql`.
- `WriteTransaction` is no longer `Send`, so it stays on the thread that began it. Moved to another thread, a second write begun there waited for itself forever.

### Fixed

- A label scan in a write transaction read the label set of every node the transaction had created, so a transaction of statements that each `MATCH (n:Label)` slowed down with every node it inserted, and 16,000 such statements took five seconds. A scan now skips the groups whose label sets lack the label.
- `SET r = {...}` and `SET r += {...}` on a relationship rewrote its whole record once per property, so replacing the properties of a relationship with thousands of them took seconds. They now write the record once.
- `SET n:A:B:...` and `REMOVE n:A:B:...` stored a label set for every label in between, which is never deleted, so removing 3,000 labels from one node grew the file by over 100 MB. They now store only the label set the node ends with.
- `SET n:A SET n:B ...` and statements that each change the labels of a node in one transaction stored a label set for every step, so adding and removing 2,000 labels one clause at a time grew the file by 142 MB. A transaction now stores only the label sets its nodes have when it commits.
- A query of many `LET` clauses took time quadratic in their number to parse and bind, and one of many `WITH` clauses took quadratic time to run, so 16,000 `LET` clauses took 30 seconds in a debug build and 32,000 `WITH` clauses 12 seconds in release. Both now take linear time.
- Reading every property of a committed node, as `properties(n)`, `keys(n)`, `SET n = {...}`, and `DELETE` do, took time quadratic in its number of properties, so a node with 50,000 properties took seconds to delete. It now takes linear time.
- A property index serves a condition whose value is any expression over earlier variables, such as `MATCH (n:L {id: i + 1})` after `UNWIND`, instead of only a literal, a parameter, a variable, or a property, so such a lookup no longer scans the label once per row.
- A deeply nested parameter or procedure result, and query text nested close to the parser's limit in a debug build, no longer overflow the stack and abort the host process. A query now fits in 1 MiB of thread stack in debug and release builds, and a value that nests lists and maps deeper than 128 levels fails with the new `ErrorKind::NestingTooDeep`.
- A regular expression nested deeper than 64 groups fails with `ErrorKind::InvalidRegex`.
- A variable-length relationship over a long path, or a pattern of many steps, no longer overflows the stack and aborts the host process, because matching keeps its search on the heap.
- A power loss while `Database::open` created a new file could leave a file that every later open refused as not a redb database. The file is now written under a staging name, synced, and linked into place.
- A thread holding a write transaction that began another write, or ran a writing query, could wait for itself forever instead of failing with `ErrorKind::WriteTransactionOnThisThread`, when another thread had committed just before it took the write lock.
- A damaged database file on which redb panics fails with `ErrorKind::CorruptData` instead of ending the host process, and every later operation on that `Database` fails the same way until the file is opened again. redb's own corruption errors are reported as `CorruptData` too, instead of `ErrorKind::Storage`.
