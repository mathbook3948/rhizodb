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
- The parser is fuzzed.
- Code and tests taken from other projects keep their source and license.

## Open

None
