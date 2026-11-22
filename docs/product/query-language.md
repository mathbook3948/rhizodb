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
- A type error that the query text alone determines fails the query before it reads data, wherever it appears in the query.
- An error that depends on values, such as overflow, division by zero, or the type of stored data, fails only when a row reaches it.
- Procedures and `CALL` are not supported.
- Patterns accept GQL's abbreviated edges `-`, `->`, and `<-`.
- Inside an expression, a single `-` between parenthesized terms is always subtraction.
- Parameters are data. They never take part in the type checks made on the query text.
- An element deleted earlier in the same query is returned as it was when deleted, and reading its properties fails.
- A `MERGE` property that evaluates to null is an error.
- `range()` has a length cap, so one query cannot exhaust the host's memory.
- Floats convert to strings in the same form as the openCypher reference implementation.
- Each function declares the argument types it accepts once, and both the check on the query text and the check on data read that declaration.
- `length()` takes a path, and `size()` takes a list or a string.
- `sum`, `avg`, `stDev`, `stDevP`, and the percentile aggregates take numbers only.
- Aggregate arguments, pattern properties and filters, pattern predicates, and pattern comprehensions fall under the early type-error rule.
- A reading clause that follows an updating clause needs a `WITH` between them.
- `toInteger()` and `toFloat()` read text only in the decimal number literal syntax with an optional sign, and give null for anything else.

## Open

- Regular expression matching with `=~`
- `EXISTS` subqueries that contain a full query
