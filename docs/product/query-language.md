# Query language

Decides which query syntax RhizoDB accepts and in what order features arrive.

## Background

GQL is the reference language, but the only public conformance suite is the openCypher TCK, and its setup queries use Cypher syntax. The accepted syntax decides how much of that suite can run unchanged.

## Decisions

- Where GQL and Cypher differ, the GQL form is canonical.
- Cypher's `CREATE` is accepted as an extension with the meaning of GQL `INSERT`, so TCK setup queries run unchanged.
- Keywords are case-insensitive, and identifiers are case-sensitive.
- The first features are `MATCH`, `OPTIONAL MATCH`, `WHERE`, `RETURN`, `WITH`, `ORDER BY`, `SKIP` or `OFFSET`, `LIMIT`, `INSERT`, and the core expressions over the stored value types.
- Further features are ordered by how many TCK scenarios they unlock.
- A pattern used as a predicate is accepted only inside a `WHERE`.
- A type error visible in the query text fails the query before it reads data. A type error that depends on data fails when a row reaches it.
- Procedures and `CALL` are not supported.
- Patterns accept GQL's abbreviated edges `-`, `->`, and `<-`. Inside an expression, a single `-` between parenthesized terms is always subtraction.
- Parameters are data. They never take part in the type checks made on the query text.
- An element deleted earlier in the same query is returned as it was when deleted, and reading its properties fails.
- A `MERGE` property that evaluates to null is an error.
- `range()` has a length cap, so one query cannot exhaust the host's memory.
- Floats convert to strings in the same form as the openCypher reference implementation.

## Open

- Regular expression matching with `=~`
- `EXISTS` subqueries that contain a full query
