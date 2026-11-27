# Streaming rows

Decides how rows pass from one read clause to the next, so that `LIMIT` and `EXISTS` stop reading once they have their rows and an aggregate holds its groups instead of its input.

## Background

The executor applied each clause to every row before the next clause began, so `MATCH ... LIMIT 1` found every match first and `UNWIND range(1, n) AS i RETURN count(*)` held one row per item. The results and errors of that order are the contract, so any other order must give exactly the same ones.

## Decisions

- A run of read clauses that ends at the last projection with a `LIMIT` or an aggregate before a table-wise `CALL` streams, passing each row on to the next clause as it comes.
- The run stops at any updating clause, so a write before a `LIMIT` applies to every row it would have applied to, and only the read clauses after it stream.
- A projection with `ORDER BY` or an aggregate holds what it needs until its input ends and then passes its rows on, and an aggregate holds one state per group.
- A projection with `ORDER BY` evaluates the sort keys of each row as it arrives and, under a `LIMIT`, holds only the `SKIP` plus `LIMIT` rows that rank first so far in a bounded heap.
- Rows that tie on every sort key rank by arrival, as in the stable full sort, so the heap keeps the same rows in the same order.
- An item error of a sorted projection is raised as its row arrives, and its first sort key error only once the input ends, since applying the clauses in turn evaluates the items of every row before any sort key.
- A plain projection with a `LIMIT` stops the clauses before it once it has its rows only when no clause since the last holding projection can fail on a row it has not read, and none of its own items can fail.
- A clause that might still fail on a later row keeps reading its input to the end, evaluating every item that might fail, and only the rows past the limit are dropped.
- An error raised where some earlier clause might still have failed on a later row makes the run start over with each clause applied in turn, which then gives the exact error.
- Whether an expression cannot fail is decided from the plan and the parameters alone: literals, parameters, variables, properties of a variable that only ever holds a node or relationship, comparisons, null tests, label tests, boolean operators, string predicates, `IN` over a fixed list, pattern predicates over such patterns, and arithmetic that cannot fail.
- Arithmetic cannot fail when each operand is a number of a known kind or null: float arithmetic and `^` always qualify, integer `+`, `-`, and `*` never do since they can overflow, integer negation only of a literal or parameter, and integer `/` and `%` qualify only by a literal or parameter divisor that is not zero, and for `/` not -1.
- A variable only ever holds an integer or null when `UNWIND` binds it from `range()` or a list literal of integers, `count()` gives it, or a projection or `LET` copies such a variable.
- A variable only ever holds a node or relationship when a pattern binds it as a node or a single relationship, or a projection or `LET` copies such a variable, since every binding has a slot of its own.
- What each variable only ever holds is decided once per plan, when the query binds, and each run decides which clauses cannot fail from that and its parameters.
- After a deletion in the same query, reading a property counts as able to fail, since the element may be gone.
- An expression that reads nothing that varies by row counts as unable to fail once one row evaluated it.
- `EXISTS`, `COUNT`, `COLLECT`, and value subqueries stream their inner clauses from the outer row, and `EXISTS` stops at the first inner row when no unread row could fail.
- A match reads its start nodes one row group at a time, as a column scan or as the row-at-a-time matcher, so a match that stops early reads only the groups it reached.
- `UNWIND range(...)` gives its integers one at a time instead of building the list, while still checking its arguments and the value limits as `range()` does.
- A run that stops early at the projection right after its match splits across threads by row group, as parallel reads decide, and any other run that stops early stays on one thread.
- Tests compare streamed runs with every clause applied in turn, on one thread and split across threads, over random graphs and queries that include failing expressions, and count the matches a run produces to check where it stops.

## Open

- The check for expressions that cannot fail does not know the type of a property, so arithmetic such as `a.k % 7`, which fails on a string, keeps a `LIMIT` reading to the end.
- Which clauses cannot fail is decided again on every run, since it depends on the parameters and on what the statement deleted.
