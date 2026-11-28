# Engines

RhizoDB against every engine the benchmark crate drives, on the same generated social graph and the same workloads as the README's table.
The embedded engines, LadybugDB, SQLite, DuckDB, and CozoDB, keep their files in a temporary directory with their default durability.
The servers, Neo4j, Memgraph, and FalkorDB, run from pinned images on the same machine, and Memgraph and FalkorDB keep their data in memory.
Every run checks that all engines return the same rows, and each engine loads through its own fastest path.

`mise run bench-servers start` starts the servers and prints the variables that name them, and `mise run bench-engines` measures again and rewrites this table.
Without those variables the table holds the embedded engines only.
The vector index workloads are left out by default, since CozoDB's HNSW index build takes longer than every other workload together, and `BENCH_ENGINES_SKIP=` runs them too.
`mise run bench-servers stop` removes the servers.

<!-- engines-table:start -->
Measured on 2026-11-28 on 4 CPUs (Intel(R) Xeon(R) Processor), Linux 6.18.44-fc-v880, at the `medium` scale (20000 persons, 200000 `KNOWS` edges, 100 cities), with 5 repetitions per workload. Times are medians in milliseconds for the whole workload, lower is better, and a row whose ranges of runs overlap is marked similar.

| Workload | RhizoDB | LadybugDB | SQLite | DuckDB | CozoDB | Neo4j | Memgraph | FalkorDB | Fastest |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| Point lookup by id | 0.516 | 0.983 | 0.302 | 46.1 | 3.94 | 48.9 | 30.5 | 16.2 | SQLite 1.7x |
| Filtered scan | 0.423 | 0.476 | 2.73 | 0.720 | 18.1 | 4.42 | 4.62 | 1.58 | similar |
| `ORDER BY ... LIMIT` | 0.727 | 0.930 | 2.72 | 0.929 | 27.4 | 7.52 | 23.5 | 5.61 | RhizoDB 1.3x |
| 1-hop neighbours | 0.418 | 0.698 | 0.078 | 8.68 | 1.31 | 11.6 | 6.04 | 4.39 | SQLite 5.3x |
| 2-hop distinct | 0.763 | 10.3 | 0.703 | 29.5 | 3.23 | 13.6 | 7.20 | 5.72 | similar |
| 3-hop distinct | 4.84 | 39.6 | 5.21 | 56.5 | 33.3 | 25.6 | 11.5 | 10.6 | RhizoDB 1.1x |
| Variable length `*1..3` | 5.31 | 33.1 | 8.85 | 125 | 31.5 | 28.1 | 13.4 | 11.6 | RhizoDB 1.7x |
| Group by city | 2.14 | 2.79 | 9.69 | 5.25 | 69.2 | 7.71 | 15.1 | 6.96 | similar |
| Substring scan of a text property | 22.9 | 27.2 | 75.1 | 59.4 | 385 | 194 | 105 | 54.5 | RhizoDB 1.2x |
| Keyword search | 53.0 | 332 | 189 | 762 | 5570 | 137 | 127 | 79.6 | RhizoDB 1.5x |
| Small commits | 66.9 | 202 | 93.4 | 610 | 33.8 | 408 | 11551 | 109 | CozoDB 2.0x |
| Bulk update | 13.0 | 12.7 | 39.6 | 8.11 | 51.0 | 47.1 | 90.2 | 7.90 | similar |
| Detach delete | 215 | 367 | 133 | 78.2 | 475 | 516 | 90.4 | 74.7 | similar |
| Bulk load | 743 | 921 | 339 | 637 | 2829 | 5726 | 3555 | 10868 | SQLite 1.9x |
| File size after bulk load | 14.3 MiB | 13.0 MiB | 16.4 MiB | 16.3 MiB | 25.5 MiB | n/a | n/a | n/a | LadybugDB 1.10x smaller |
<!-- engines-table:end -->

A row times a whole batch of operations, so compare engines within a row.
A server answers over a local socket, so its times include a round trip that an embedded engine does not pay.
An engine whose language has no form of a workload shows `unsupported`, and a server reports no file size.
