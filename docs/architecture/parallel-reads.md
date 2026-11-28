# Parallel reads

Decides how one read query runs on several threads and how it stays identical to running on one.

## Background

A read transaction sees one snapshot, so threads can share it without coordination. Scans, aggregates, and traversals that start from every node of a label spend their time per start node, which splits across cores.

## Decisions

- A clause splits only when it is a non-optional `MATCH` fed by exactly one row, whose first pattern starts from an unbound node that no property index or ID lookup can serve.
- A clause that a scan of relationship chunks answers does not split, since that scan reads every chunk once on the calling thread, and a scan that stops hands the clause to the split as if it had not run.
- The unit of work is a morsel, a contiguous run of the start nodes in ID order, and each thread takes the next morsel from a shared counter until none are left.
- A morsel holds whole row groups, so each worker reads a group's columns once, and a row group is cut into equal parts only when there are fewer groups than threads.
- A morsel restricts the start nodes of that one pattern part, found by identity, so later patterns, subqueries, and pattern predicates read the whole graph as before.
- Each worker evaluates its morsels with the operators the serial path uses, the column scan, the hop aggregate, and the row-at-a-time matcher, through one environment of its own that shares only the transaction, the parameters, and the clocks, and keeps its caches across its morsels.
- An aggregating projection after the match merges per-morsel groups in morsel order, so groups keep the order of their first row and every aggregate sees its values in scan order.
- A worker keeps the values of `sum`, `avg`, and the distinct aggregates other than `min` and `max` instead of folding them, and the merge folds them in scan order, so float sums, integer overflow, and `DISTINCT` give exactly the serial result.
- A worker hands on one value for each `min` and `max` and its collected list for `collect`, and the merge keeps the value or appends the list as if the worker's rows followed the rows merged before them.
- A plain projection runs per morsel without `SKIP` or its trailing filter and with a `LIMIT` of `SKIP` plus `LIMIT`, and the joined rows then go through the full projection once, which keeps ties in scan order.
- With `LIMIT` and neither `ORDER BY` nor `DISTINCT`, no morsel starts once the finished morsels before it hold the rows the projection keeps.
- A match whose streamed run stops early at a `LIMIT` in the projection right after it splits by row group, with morsel `n` being row group `n`, so no start node is listed before the split and no row group past the ones the `LIMIT` needs is read.
- Such a split happens only when some node ID reaches the threshold, and it reads row groups on the calling thread alone until it has read the threshold's start nodes, taking helpers only if the `LIMIT` is not met by then.
- A worker that takes a row group without start nodes moves the shared counter on to the next group that has some, so a run of empty groups is read about once.
- A match whose streamed run stops early at a later `LIMIT` does not split, since the serial run then reads only the start nodes it needs.
- A `DISTINCT` projection without `LIMIT` stays on the calling thread, since joining the morsels would repeat its whole deduplication there.
- A match that feeds any other clause returns its rows per morsel, joined in morsel order.
- A table-wise `CALL`, which heads each block of a `NEXT` chain, never runs per morsel, since it reads all its input rows at once.
- A worker evaluates at least what the serial path would on its morsel, so any error the serial path raises also fails some worker.
- When any worker or the merge fails, the clause runs again on the calling thread, which then returns the serial result or error exactly.
- The calling thread works on morsels too, and a helper thread that cannot be spawned is skipped.
- Helper threads get a 2 MiB stack.
- A worker that panics re-raises its panic on the calling thread.
- A query that calls a host procedure, any updating query including an updating `CALL` or `NEXT` block, any query in a write transaction, and `MATCH (n) RETURN count(n)` answered from node counts stay on the calling thread.
- `OpenOptions::threads` sets the threads per query, defaulting to the available parallelism, and one or zero keeps every query on the calling thread.
- `OpenOptions::parallel_threshold` sets the start nodes a match needs before it splits, or that a split by row group reads alone before it takes helpers, defaulting to 8192, which is two row groups.
- A threshold below two acts as two, since one start node cannot split.
- Consecutive row groups join one morsel up to a quarter of a thread's share of the start nodes.
- Threads are scoped to the query and spawned per split clause, with no pool.
- Each database counts the threads running its read queries, callers and helpers alike, and a clause splits only when that count leaves some of `threads` idle, taking at most the idle ones as helpers.
- A clause that finds no idle thread runs on the calling thread without collecting its start nodes for morsels.
- Tests compare every split query with the serial path on random graphs, and the whole integration suite, the reference evaluator among it, also runs with splitting forced at a threshold of two in `mise run random-deep`.

## Open

- A worker keeps every value of `sum`, `avg`, and the distinct aggregates other than `min` and `max`, so their memory grows with the rows instead of the groups.
- A traversal from a single start node, such as a seek followed by several hops, does not split yet.
