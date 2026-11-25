# Parallel reads

Decides how one read query runs on several threads and how it stays identical to running on one.

## Background

A read transaction sees one snapshot, so threads can share it without coordination. Scans, aggregates, and traversals that start from every node of a label spend their time per start node, which splits across cores.

## Decisions

- A clause splits only when it is a non-optional `MATCH` fed by exactly one row, whose first pattern starts from an unbound node that no property index or ID lookup can serve.
- The unit of work is a morsel, a contiguous run of the start nodes in ID order, and each thread takes the next morsel from a shared counter until none are left.
- A morsel holds whole row groups, so each worker reads a group's columns once, and a row group is cut into equal parts only when there are fewer groups than threads.
- A morsel restricts the start nodes of that one pattern part, found by identity, so later patterns, subqueries, and pattern predicates read the whole graph as before.
- Each worker evaluates its morsels with the operators the serial path uses, the column scan, the hop aggregate, and the row-at-a-time matcher, through one environment of its own that shares only the transaction, the parameters, and the clocks, and keeps its caches across its morsels.
- An aggregating projection after the match merges per-morsel groups in morsel order, so groups keep the order of their first row and every aggregate sees its values in scan order.
- A worker keeps the values of `sum`, `avg`, and the distinct aggregates instead of folding them, and the merge folds them in scan order, so float sums, integer overflow, and `DISTINCT` give exactly the serial result.
- A plain projection runs per morsel without `SKIP` or its trailing filter and with a `LIMIT` of `SKIP` plus `LIMIT`, and the joined rows then go through the full projection once, which keeps ties in scan order.
- With `LIMIT` and neither `ORDER BY` nor `DISTINCT`, no morsel starts once the finished morsels before it hold the rows the projection keeps.
- A match that feeds any other clause returns its rows per morsel, joined in morsel order.
- A worker evaluates at least what the serial path would on its morsel, so any error the serial path raises also fails some worker.
- When any worker or the merge fails, the clause runs again on the calling thread, which then returns the serial result or error exactly.
- The calling thread works on morsels too, and a helper thread that cannot be spawned is skipped.
- Helper threads get a 2 MiB stack.
- A worker that panics re-raises its panic on the calling thread.
- A query that calls a host procedure, any query in a write transaction, and `MATCH (n) RETURN count(n)` answered from node counts stay on the calling thread.
- `OpenOptions::threads` sets the threads per query, defaulting to the available parallelism, and one or zero keeps every query on the calling thread.
- `OpenOptions::parallel_threshold` sets the start nodes a match needs before it splits, defaulting to 8192, two row groups.
- Consecutive row groups join one morsel up to a quarter of a thread's share of the start nodes.
- Threads are scoped to the query and spawned per split clause, with no pool.
- Tests compare every split query with the serial path on random graphs, and the whole suite and the reference evaluator also run with splitting forced at a threshold of two.

## Open

- A worker keeps every value of `sum`, `avg`, and the distinct aggregates, so their memory grows with the rows instead of the groups.
- A traversal from a single start node, such as a seek followed by several hops, does not split yet.
