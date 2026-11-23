# Verification

Decides how RhizoDB proves its query results and durability are correct.

## Background

A database that returns wrong results or loses data after a crash is worse than none. Correctness is checked against external references rather than only against our own tests.

## Decisions

- The parts of GQL that overlap with Cypher are verified with the openCypher TCK.
- The TCK is vendored unchanged, with its license and the upstream commit it came from.
- A checked-in list names the TCK scenarios that must pass.
- A scenario that starts or stops passing fails the run until the list is updated.
- TCK error expectations match on error kind and detail, not on whether the error is raised at compile time or at runtime.
- GQL features outside the Cypher overlap are verified with hand-written scenarios in the TCK's format, run by the same runner.
- Each GQL scenario sits in a directory named for the ISO/IEC 39075 clause it checks, and its feature cites that clause.
- The GQL scenarios have their own checked-in list of scenarios that must pass, under the same rule as the TCK's.
- GQL scenarios also cover GQL syntax RhizoDB does not accept yet, so the list shows what is missing.
- The same queries run on LadybugDB and the results are compared.
- LadybugDB is built from the source of a pinned release, never from a prebuilt library it would download.
- The comparison lives in its own crate outside the main workspace, with its own CI workflow.
- The comparison runs in CI on Linux only.
- The comparison crate has its own license policy, which allows the main list plus named exceptions for crates only LadybugDB pulls in.
- The corpus joins a hand-written file, queries generated from a fixed seed, and the TCK scenarios that need no graph.
- Hand-written and generated queries run on one generated graph, which LadybugDB gets with a schema declared before the data loads.
- Every node of the generated graph has an integer primary key, which RhizoDB stores as an ordinary property.
- TCK scenarios that build or read a graph stay out of the corpus, since LadybugDB would need a schema derived for each one.
- Rows are compared as a multiset unless the query orders by every returned column.
- Column names are not compared.
- A null property of a LadybugDB node or relationship counts as absent.
- A checked-in list names every query on which the engines do not agree, and a run fails when an outcome differs from it.
- Each listed difference in returned rows carries the reason the engines differ.
- A query that only one engine accepts is listed with the error it raised.
- Consistency after a forced shutdown is tested.
- Every storage backend passes one conformance suite written against the storage trait.
- Every storage backend is checked against a model of its committed versions over random sequences of transactions, reads, and reopens.
- The own engine's behavior under power loss and I/O errors is tested on the simulated file system.
- The redb backend also runs on the simulated file system, with sector writes that a power loss keeps whole, since redb relies on that.
- Node meta is checked against a model of node labels over random sequences of node and label edits, commits, rollbacks, reopens, and power losses.
- A commit that writes node meta is failed at each of its storage operations in turn, and the graph recovered from every power loss must be the one before or after it.
- Every format upgrade is tested on a fixture that a checked-in program writes with the last commit of the older format.
- CI runs each randomized storage test with a fixed number of cases, and a nightly workflow runs a hundred times as many on every CI platform.
- A failing case a randomized test finds is checked in, and every later run replays it.
- Power loss under a real file system is tested on LazyFS in a nightly job, with no device-mapper targets.
- The parser, the value decoder, damaged database files, and random write transactions are fuzzed.
- A damaged database file must make opening or the query fail with an error, never panic or hang.
- Every input the fuzzer found a failure with is checked in and replayed by an integration test.
- The nightly fuzz run skips panics raised inside redb on damaged files and fails on any other.
- Isolation is checked by recording histories of concurrent list-append transactions and requiring their dependencies and real-time order to form no cycle.
- Durability through the public API is tested by killing a writing child process and checking that the reopened file holds every reported commit whole, nothing uncommitted, and indexes that agree with the data.
- Random write transactions through GQL are compared after each transaction with a model of the graph, together with read queries the model answers on its own.
- Kuzu test files on the empty dataset are vendored unchanged with their license and upstream commit, and their cases run under a checked-in list of cases that must pass, by the TCK's rule.
- The Kuzu runner lets statements that only declare a schema succeed without running, and an expected error matches any error.
- Code and tests taken from other projects keep their source and license.

## Open

None
