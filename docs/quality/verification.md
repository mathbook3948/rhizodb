# Verification

Decides how RhizoDB proves its query results and durability are correct.

## Background

A database that returns wrong results or loses data after a crash is worse than none. Correctness is checked against external references rather than only against our own tests.

## Decisions

- The parts of GQL that overlap with Cypher are verified with the openCypher TCK.
- The same queries run on LadybugDB and the results are compared.
- Consistency after a forced shutdown is tested.
- The parser is fuzzed.
- Code and tests taken from other projects keep their source and license.

## Open

None
