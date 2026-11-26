# Changelog

All notable changes to `rhizodb` are recorded here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html). A released section's heading is exactly `## <version>`, such as `## 0.1.0`, because the release workflow reads it as the release notes.

## Unreleased

### Added

- Embedded replication. `Database::changes_since` returns the whole transactions after another database's `Database::position` as `Changes`, which travel as checksummed bytes and `Database::apply` logs on a replica, and replicas relay changes to further replicas. A `Position` holds a term, a writer id, an LSN, a chain hash over the history, and the id of the database the history belongs to, so two histories that reuse a term are told apart and a replica never follows an unrelated database.
- `Database::promote` makes a node the writer of a higher term, and `Database::demote` hands writing over without losing a write, waiting a given time for an open write transaction. A file remembers its role, which `Database::role` reports as a `Role`: a writer reopens fenced until the host promotes it again, and `OpenOptions::replica` creates a replica.
- `Database::history` returns the terms a database followed as a `History`, which `Database::compare_lineage` compares with another database's to tell which side catches up and which must resync, as a `Lineage`. Each `HistoryEntry` names its term, writer id, first and last LSN, and, with `HistoryEntry::incarnation`, the database file that began the term, so two files that began one term under one writer id are told apart.
- New error kinds `NotWriter`, `NotReplica`, `Fenced`, `Diverged`, `DifferentDatabase`, `TooFarBehind`, `Gap`, and `WriterBusy`, and `Error::replication_failure`, which returns the positions and LSNs behind a replication error as a `ReplicationFailure`.
- `Database::snapshot` pins the last checkpoint as a `Snapshot`, a stream of checksummed chunks read at any offset with `Snapshot::read_at`, so a transfer that is cut off resumes where it stopped. Writes, deletes, and compaction go on while it is read, and taking one forces no checkpoint.
- `Database::install_snapshot` and `OpenOptions::install_snapshot` write a snapshot stream to a database file and open it as a replica. The stream is synced and checked page by page before it replaces the file, so a crash or a failure leaves the old database or the whole snapshot, and a damaged chunk fails as `ErrorKind::CorruptData`.
- `Database::acknowledge` keeps the write-ahead log after a peer's `Position` for that peer, and `Database::forget_peer` releases it. `OpenOptions::max_retained_bytes`, 1 GiB by default, caps the log kept for peers and snapshots, past which the peers furthest behind are released.
- `ErrorKind::InvalidPeerId` for a peer id that is empty or longer than 64 bytes.
- Acknowledged peers and the log kept for them survive a clean restart and a crash: the database file records each peer's position with every checkpoint, and a reopened database keeps the log segments those peers still need, so a restarted writer does not force its replicas to a new snapshot. A database keeps the log for at most 32 peers, and acknowledging another fails with the new `ErrorKind::TooManyPeers`.
- `OpenOptions::force_install`, without which an install refuses, with `ErrorKind::Fenced`, a snapshot older in term than the database it replaces has seen, with `ErrorKind::DifferentDatabase`, a snapshot of another database than the one it replaces, and, with `ErrorKind::CorruptData`, to replace a database whose history of terms cannot be read. An installed database is a replica of a fresh incarnation that keeps the newest term either side has seen, so changes of a replaced writer are fenced. `Database::acknowledge` with a position of a newer term fences a writer.
- `OpenOptions::snapshot_time_limit`, one hour by default, after which the database releases a `Snapshot` still alive and reading it fails with the new `ErrorKind::SnapshotExpired`, and `DatabaseStats::held_bytes`, the space that open snapshots and read transactions keep the file from reusing.
- `OpenOptions::max_value_items` and `OpenOptions::max_value_bytes` set how many list items and map entries, and how many bytes of strings, one value that a query builds may hold, counted through every nested list and map. They default to 10,000,000 items and 256 MiB, and `range()` gives at most `max_value_items` elements.

### Changed

- A file records engine format minor 4, which adds the replication history, the role, and the log kept for peers, once this build opens it, so builds before it refuse it with `UnsupportedFormatVersion`.
- A `MATCH` pattern starts at a later node that an earlier clause binds, or that an ID lookup or a property index serves, instead of reading every node at its left end and checking the far end of each walk, so `MATCH (x:Person {id: $id}) MATCH (a)-[:FOLLOWS]->(x)` and `MATCH (a)-[:KNOWS]->(b:Person {id: $x})` with an index on `Person.id` read only the relationships of the found node. A later lookup is used only when it finds fewer nodes than the leftmost start. The rows are the same, though rows without `ORDER BY` may come in another order, and since a property map is evaluated only for the elements the walk reaches, a moved start can change whether a query raises a value error such as division by zero.
- Returning a node or a relationship reads each label, relationship type, and property key name from storage once per database (once per transaction inside a write transaction) instead of once per element, so returning many elements no longer reads storage once per label and property.
- `labels(n)`, `keys(n)`, `keys(r)`, and `type(r)` no longer decode the element's property values, so they cost the same on elements with large text or vector properties as on small ones.
- Reading or changing the relationships of a node no longer reads the stored relationships of a hub node in the same block of 16 node IDs. Before, every such read or commit read the whole of the hub's adjacency, so its cost grew with the hub's degree.
- A commit that adds a relationship to a hub no longer reads the hub's stored adjacency, and rewrites it once its pending changes reach the square root of eight times its size instead of half a part. Adding relationships to one node one commit at a time no longer takes time quadratic in its degree, and the bytes it writes for the hub grow with the degree to the power 1.5 instead of the square.
- Reading a relationship whose chunk of 32 records is longer than 1 KiB, as with a few properties or one short string per relationship, no longer reads the database file each time. The block cache now keeps long values whole beside its pages, within the same memory budget, so repeated reads of such relationships, traversals that read their properties, and scans such as `MATCH ()-[r]->() RETURN avg(r.weight)` take no file read or checksum once the chunk is cached. `DatabaseStats::cache_hits` and `cache_misses` now count these reads too. A scan that reads more than 1 MiB keeps the rest of what it reads only in free room of the cache, so a scan of more data than the cache holds, such as over long text properties, does not push out the pages and values that lookups and traversals keep reading.
- `min()` and `max()`, with or without `DISTINCT`, keep one value per group as rows arrive instead of holding every value of the group until the end, so `MATCH (n:N) RETURN min(n.x), max(n.x)` uses memory per group instead of per row and no longer slows down when it splits across threads. The results are unchanged, ties between equivalent values such as `1` and `1.0` included.
- Joining the `collect()` lists of a query split across threads checks the value limits once per list instead of once per item, and `percentileCont()` and `percentileDisc()` sort their values once instead of twice.

### Fixed

- A short query could still abort the host process by multiplying values that each stayed under the length caps, as in `[x IN range(1, 10000000) | range(1, 10000000)]`, `collect(range(1, 1000))` over many rows, or a `reduce` that appends a new list at every step. List comprehensions, pattern comprehensions, `COLLECT` subqueries, `collect()`, list and map literals, `CAST`, `split()`, `toUpper()`, `toLower()`, and `normalize()` now fail with `ErrorKind::NumberOutOfRange` once their value passes the limits, and the limits count the items and strings of nested lists and maps too.
- A `reduce` that appends to its accumulator, as in `reduce(l = [], i IN range(1, 1000000) | l + i)` or the same with strings, copied the accumulator at every step and took time quadratic in its length. It now grows the accumulator in place.

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
