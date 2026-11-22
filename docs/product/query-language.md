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
- `EXISTS { query }` runs the query from the current row with the outer variables in scope, and is true when the query gives a row.
- A query inside `EXISTS` may end without `RETURN`.
- A query inside `EXISTS` cannot update the graph.
- A query inside `EXISTS` keeps the variables it binds to itself.
- A variable that a projection hides from the expressions next to it is hidden inside both forms of `EXISTS` there.
- A type error that the query text alone determines fails the query before it reads data, wherever it appears in the query.
- An error that depends on values, such as overflow, division by zero, or the type of stored data, fails only when a row reaches it.
- The check on the query text reads a value error as null and goes on.
- The check reports a type error only when no value read as null was computed in the same expression step, so it reports only an error that evaluating the expression would really raise.
- A type error the check skips this way still fails when a row reaches it.
- A procedure is a Rust function the host registers on a `Database` under a qualified name.
- `CALL` runs a registered procedure.
- A host procedure sees only its arguments, never the graph.
- The engine provides built-in procedures in the reserved `db` namespace, and they read the graph of the transaction that calls them.
- A host cannot register a procedure in the `db` namespace.
- `CALL` is a reading clause, because no procedure writes the graph.
- Procedure names are case-sensitive.
- Each procedure parameter declares the types it accepts.
- Procedure arguments fall under the same checks as function arguments.
- Every procedure parameter accepts null.
- An integer argument to a float parameter arrives as a float.
- A query that is a single `CALL` returns the `YIELD` items, or every output without `YIELD` or with `YIELD *`.
- A query that is a single `CALL` may leave out the arguments, which are then read from the query parameters of the same names.
- A `CALL` that is the only clause inside `EXISTS` follows the rules of a `CALL` inside a larger query.
- A `CALL` inside a larger query passes the arguments of a procedure that takes any.
- A `CALL` inside a larger query binds only its `YIELD` items.
- A `CALL` inside a larger query rejects `YIELD *`.
- A `CALL` gives one row per row the procedure returns.
- A `CALL` of a procedure without outputs keeps each row once.
- Patterns accept every GQL edge form, full and abbreviated: `<-`, `->`, `-`, `<->`, `~`, `<~`, and `~>`.
- Every stored relationship is directed, so `~` matches no relationship, and `<~` and `~>` match what `<-` and `->` match.
- A variable-length `~` edge still matches a path of zero hops.
- `<~>` and `<~[]~>` are syntax errors.
- A pattern that creates rejects the edge forms with `~` as missing a direction, and `INSERT` also rejects `-` and `<->`.
- A label test on a relationship treats its type as its only label, so `r:T` is true when the type is `T`.
- Label expressions take `|`, `&`, `!`, `%`, and parentheses, with `|` binding weakest and `!` tightest.
- `!` applies to a label, `%`, or a parenthesized expression, so `!!A` is a syntax error.
- `%` is true of an element with at least one label, so it is true of every relationship.
- A node or relationship pattern introduces its label expression with `:` or `IS`.
- `IS` starts a label expression in a pattern only when a label expression follows it, so a variable can still be named `is`.
- Cypher's `:A:B` is the conjunction of `A` and `B`, and it only joins plain label names, so mixing it with another label operator is a syntax error.
- A relationship pattern takes a single `:`, so `[:T:U]` is a syntax error.
- A `:` may follow a `|`, as in Cypher's `[:T|:U]`.
- A label test in an expression takes a label expression after `:` or after `IS LABELED`, and `IS NOT LABELED` is its negation.
- `IS LABELED` and `IS NOT LABELED` bind like `IS NULL`.
- Inside the `WHERE` of a list or pattern comprehension, a `|` outside parentheses or brackets starts the projection, so a label disjunction there needs parentheses.
- A node in a pattern that creates takes only a label or a conjunction of labels, and anything else fails with its own error kind.
- Inside an expression, a single `-` between parenthesized terms is always subtraction.
- A malformed number literal is an invalid number where a value is expected, and an unexpected token anywhere else.
- Parameters are data. They never take part in the type checks made on the query text.
- An element deleted earlier in the same query is returned as it was when deleted.
- Reading the properties of an element deleted earlier in the same query fails.
- The type of a relationship deleted earlier in the same query stays readable, through `type()` and label tests.
- Reading the labels of a node deleted earlier in the same query fails.
- A `MERGE` property that evaluates to null is an error.
- `range()` has a length cap, so one query cannot exhaust the host's memory.
- Floats convert to strings in the same form as the openCypher reference implementation.
- Each function declares the argument types it accepts once, and both the check on the query text and the check on data read that declaration.
- `length()` takes a path, and `size()` takes a list or a string.
- An argument of `range()` that is not an integer fails with its own error kind, under the same early type-error rule.
- Every other function argument of the wrong type, including a non-integer one where only an integer fits, fails as an ordinary argument type error.
- `sum`, `avg`, `stDev`, `stDevP`, and the percentile aggregates take numbers only.
- An aggregate argument cannot call `rand()`.
- A percentile aggregate over a group with no rows is null and does not evaluate its percentile.
- A null percentile makes a percentile aggregate null.
- Operators, indexing, slicing, and label tests fall under the early type-error rule for every operand whose type the query text fixes, including node, relationship, and path variables.
- The type the query text fixes leaves out null, so an operand that may be null, such as a comparison or an `OPTIONAL MATCH` variable, still fails early when no value of its type fits.
- Aggregate arguments, pattern properties and filters, pattern predicates, and pattern comprehensions fall under the early type-error rule.
- A reading clause that follows an updating clause needs a `WITH` between them.
- `FILTER` keeps the rows whose condition is true, with or without `WHERE` after the keyword, and keeps every variable in scope.
- A `FILTER` condition is a `WHERE` in every rule above, so it may hold a pattern predicate and may not aggregate.
- `FILTER` is a reading clause, so it cannot end a query and does not separate an updating clause from a reading clause after it.
- `LET` binds each value to a new variable and keeps every variable in scope.
- A `LET` variable cannot already be in scope, so `LET` never shadows or rebinds a variable.
- The values of one `LET` see only the incoming row, not the variables the same `LET` binds.
- A `LET` value cannot aggregate.
- `LET` is a reading clause under the same clause-order rules as `FILTER`.
- `LET` accepts only `variable = value`, without the `VALUE` keyword or a declared type.
- A `WITH` item without an alias is reported only after the projection's expressions, including its `ORDER BY`, bind without error.
- A column projected twice is reported after a `WITH` item without an alias.
- `split()` by the empty string gives one string per character, so it gives an empty list for the empty string.
- `toInteger()` and `toFloat()` read text only in the decimal number literal syntax with an optional sign and optional surrounding whitespace, and give null for anything else.
- Temporal values follow openCypher: `date`, `localtime`, `time`, `localdatetime`, `datetime`, and `duration` build them from a map, a string, or another temporal value.
- The temporal functions `truncate`, `transaction`, `statement`, `realtime`, `fromepoch`, `fromepochmillis`, `between`, `inMonths`, `inDays`, and `inSeconds` exist under the openCypher names.
- A temporal value built without a time zone gets UTC.
- The `statement` clock and the clock of a call without arguments read one instant per query, so two calls in one query agree.
- The `transaction` clock reads one instant per transaction, as decided in the transactions doc.
- The `realtime` clock reads the system clock at each call.
- Temporal values convert to strings in the ISO 8601 forms the TCK expects.
- A temporal function reads back every string it produces.
- A fractional duration component carries into the next smaller units, with a month of 30.436875 days.
- The carry is exact and only the nanoseconds are truncated, except where one of the limits below first truncates an amount toward zero.
- A duration string amount keeps its whole part exactly and drops only fraction digits worth less than a nanosecond in any unit.
- A float that multiplies or divides a duration counts as the shortest decimal that reads back as it.
- A duration multiplied by a number keeps a fixed number of decimal places of each unit, set in code.
- A duration divided by a number too large for an exact fraction keeps the same fixed number of decimal places of each unit.
- A duration divided by a number so large that the quotient is far below a nanosecond is the zero duration.
- Any other product or quotient that fits in the duration range is computed exactly.
- A duration divided by an infinite float is the zero duration.
- A duration divided by NaN, or multiplied by an infinite float or NaN, raises an invalid temporal value error, even when the duration is zero.
- Adding a duration to a temporal value adds months, then days, then seconds and nanoseconds, and drops the units the value does not have.
- A date moves by the whole days in the seconds and nanoseconds of a duration, truncated toward zero.
- `<`, `<=`, `>`, and `>=` compare two temporal values of the same type and give null otherwise.
- Times and datetimes compare by instant alone, so the same instant at two offsets or zones is neither less nor greater, even though the two are not equal.
- Durations do not compare.
- Two times or two datetimes are equal when they have the same instant and the same offset or zone.
- Two durations are equal when their months, days, seconds, and nanoseconds are.
- `ORDER BY` places temporal values after paths and before strings.
- `ORDER BY` sorts temporal types in the order datetime, local datetime, date, time, local time, duration.
- Within a temporal type, `ORDER BY` sorts by instant or by value, and then by offset and zone.
- A temporal argument that does not parse, a map key a temporal function does not know or cannot combine, and a component outside its range fail with their own error kind.
- That error kind depends on values and never on the query text alone.
- `=~` is true when the regular expression on its right matches the whole string on its left.
- Regular expressions follow the syntax of the Rust `regex` crate, so lookaround and backreferences are not accepted.
- Matching takes time linear in the length of the string, whatever the pattern.
- `=~` gives null when either operand is null or not a string, like `STARTS WITH`, so it raises no type error.
- A pattern is compiled only when both operands are strings.
- A pattern that is not a valid regular expression fails with its own error kind.
- A pattern whose compiled form passes a size limit set in code fails with the same error kind.
- That error kind depends on values and never on the query text alone.

## Open

None
