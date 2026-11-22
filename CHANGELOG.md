# Changelog

All notable changes to `rhizodb` are recorded here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html). A released section's heading is exactly `## <version>`, such as `## 0.1.0`, because the release workflow reads it as the release notes.

## Unreleased

### Added

- Embedded property graph storage on redb, with a versioned on-disk format that upgrades older databases on open.
- GQL queries through `Database::execute`, with Cypher `CREATE` accepted as GQL `INSERT`.
- Temporal values: dates, times, datetimes with zones, and durations.
- Host-registered procedures callable with `CALL`, outside the `db` namespace that built-in procedures reserve, and full `EXISTS { ... }` subqueries.
- Regular expression matching with `=~`.
- Explicit read and write transactions through `Database::begin_read` and `Database::begin_write`.
- Keyword (BM25) and exact vector indexes, kept in the same transaction as the data they index, created with `Database::create_keyword_index` and `Database::create_vector_index`, and searched with `CALL db.index.keyword.query` and `CALL db.index.vector.query`.
