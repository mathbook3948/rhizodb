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
- A percentile aggregate over a group with no rows is null and does not evaluate its percentile.
- A null percentile makes a percentile aggregate null.
- Operators, indexing, slicing, and label tests fall under the early type-error rule for every operand whose type the query text fixes, including node, relationship, and path variables.
- Aggregate arguments, pattern properties and filters, pattern predicates, and pattern comprehensions fall under the early type-error rule.
- A reading clause that follows an updating clause needs a `WITH` between them.
- `split()` by the empty string gives one string per character, so it gives an empty list for the empty string.
- `toInteger()` and `toFloat()` read text only in the decimal number literal syntax with an optional sign and optional surrounding whitespace, and give null for anything else.

- Temporal values follow openCypher: `date`, `localtime`, `time`, `localdatetime`, `datetime`, and `duration` build them from a map, a string, or another temporal value, and the `truncate`, `transaction`, `statement`, `realtime`, `fromepoch`, `fromepochmillis`, `between`, `inMonths`, `inDays`, and `inSeconds` variants exist under those names.
- A temporal value built without a time zone gets UTC.
- The `transaction` and `statement` clocks and the clock of a call without arguments read one instant per query, so two calls in one query agree. The `realtime` clock reads the system clock at each call.
- Temporal values convert to strings in the ISO 8601 forms the TCK expects, and a temporal function reads back every string it produces.
- A fractional duration component carries into the next smaller units, with a month of 30.436875 days, and the carry is exact before the nanoseconds are truncated.
- Adding a duration to a temporal value adds months, then days, then seconds and nanoseconds, and drops the units the value does not have.
- A date moves by the whole days in the seconds and nanoseconds of a duration, truncated toward zero.
- `<`, `<=`, `>`, and `>=` compare two temporal values of the same type and give null otherwise.
- Times and datetimes compare by instant alone, so the same instant at two offsets or zones is neither less nor greater, even though the two are not equal.
- Durations do not compare.
- Two times or two datetimes are equal when they have the same instant and the same offset or zone. Two durations are equal when their months, days, seconds, and nanoseconds are.
- `ORDER BY` places temporal values after paths and before strings, in the order datetime, local datetime, date, time, local time, duration. Within a type it sorts by instant or by value, and then by zone.
- A temporal argument that does not parse, a map key a temporal function does not know or cannot combine, and a component outside its range fail with their own error kind, which depends on values and never on the query text alone.

## Open

- Regular expression matching with `=~`
- `EXISTS` subqueries that contain a full query
