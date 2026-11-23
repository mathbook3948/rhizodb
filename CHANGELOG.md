# Changelog

All notable changes to `rhizodb` are recorded here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html). A released section's heading is exactly `## <version>`, such as `## 0.1.0`, because the release workflow reads it as the release notes.

## Unreleased

### Added

- Embedded property graph storage on redb, with a versioned on-disk format that upgrades older databases on open.
- Node labels kept as interned label sets in one node meta chunk per 4096 node IDs, so label tests read no node record and `MATCH (n:Label) RETURN count(*)` reads one chunk per 4096 nodes.
- `count(n)` and counts of `MATCH (n)` without a label read the node meta counts as `MATCH (n:Label) RETURN count(*)` does, and a commit that changes a few nodes of a row group writes a small delta per node instead of the group's whole chunk.
- Node meta carries checksums, so a damaged chunk or delta fails the read as corrupt data instead of answering wrongly.
- Node properties kept in one column per property key and 4096 node IDs, typed when its values share a type, so a scan, filter, sort, or aggregate reads only the properties it uses, a commit that changes a few nodes writes a small checksummed delta per value, and a damaged column fails the read as corrupt data. Databases of earlier formats move their node records into columns on open.
- GQL queries through `Database::execute`, with Cypher `CREATE` accepted as GQL `INSERT`.
- Temporal values: dates, times, datetimes with zones, and durations.
- Host-registered procedures callable with `CALL`, outside the `db` namespace that built-in procedures reserve, and full `EXISTS { ... }` subqueries.
- Regular expression matching with `=~`.
- GQL linear statements `FILTER` and `LET`, label expressions with `|`, `&`, `!`, `%`, and parentheses in patterns and in `IS [NOT] LABELED`, label sets after `IS` or `&` in `SET` and `REMOVE`, and every GQL edge orientation, including `~`, `<~`, and `~>`.
- Explicit read and write transactions through `Database::begin_read` and `Database::begin_write`.
- Keyword (BM25) and exact vector indexes, kept in the same transaction as the data they index, created with `Database::create_keyword_index` and `Database::create_vector_index`, and searched with `CALL db.index.keyword.queryNodes` and `CALL db.index.vector.queryNodes`.
- Keyword and vector indexes over the properties of relationships of one type, created by passing `IndexTarget::RelationshipType` to `Database::create_keyword_index` and `Database::create_vector_index`, and searched with `CALL db.index.keyword.queryRelationships` and `CALL db.index.vector.queryRelationships`, which yield `relationship` and `score`.
- Keyword queries match `"quoted phrases"` and `prefix*` terms, compare text in Unicode normalization form NFKC, and find words inside Thai, Lao, Khmer, and Myanmar text.
- Property indexes over one property of the nodes of a label or the relationships of a type, created with `Database::create_property_index`, which `MATCH` and `MERGE` read for `=`, `IN`, `<`, `<=`, `>`, and `>=` on that property, and `id()` lookups in `WHERE` that read an element by ID instead of scanning.

### Fixed

- A variable-length relationship over a long path, or a pattern of many steps, no longer overflows the stack and aborts the host process, because matching keeps its search on the heap.
- A damaged database file on which redb panics fails with `ErrorKind::CorruptData` instead of ending the host process, and every later operation on that `Database` fails the same way until the file is opened again. redb's own corruption errors are reported as `CorruptData` too, instead of `ErrorKind::Storage`.
