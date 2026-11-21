# Verification

Decides how RhizoDB proves its query results and durability are correct.

## Background

A database that returns wrong results or loses data after a crash is worse than none. Correctness is checked against external references rather than only against our own tests.

## Decisions

- The parts of GQL that overlap with Cypher are verified with the openCypher TCK.
- The TCK is vendored unchanged, with its license and the upstream commit it came from.
- A checked-in list names the TCK scenarios that must pass. A scenario that starts or stops passing fails the run until the list is updated.
- TCK error expectations match on error kind and detail, not on whether the error is raised at compile time or at runtime.
- The same queries run on LadybugDB and the results are compared.
- Consistency after a forced shutdown is tested.
- The parser is fuzzed.
- Code and tests taken from other projects keep their source and license.

## Open

- How TCK setup queries written in Cypher-only syntax, such as `CREATE`, are run
- How GQL features outside the Cypher overlap are verified
