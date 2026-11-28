# Engines

RhizoDB against every engine the benchmark crate drives, on the same generated social graph and the same workloads as the README's table.
The embedded engines, LadybugDB, SQLite, DuckDB, and CozoDB, keep their files in a temporary directory with their default durability.
The servers, Neo4j, Memgraph, and FalkorDB, run from pinned images on the same machine, and Memgraph and FalkorDB keep their data in memory.
Every run checks that all engines return the same rows, and each engine loads through its own fastest path.

`mise run bench-servers start` starts the servers and prints the variables that name them, and `mise run bench-engines` measures again and rewrites this table.
Without those variables the table holds the embedded engines only.
The vector index workloads are left out by default, since CozoDB's HNSW index build takes longer than every other workload together, and `BENCH_ENGINES_SKIP=` runs them too.
`mise run bench-servers stop` removes the servers.
This table comes from its own run, apart from the README's, so a row the two share can differ between them.

<!-- engines-table:start -->
Measured on 2026-11-28 on 4 CPUs (Intel(R) Xeon(R) Processor), Linux 6.18.44-fc-v880, at the `medium` scale (20000 persons, 200000 `KNOWS` edges, 100 cities), with 5 repetitions per workload. Times are medians in milliseconds for the whole workload, lower is better, and a row is marked similar when its ranges of runs overlap, or, for the file size, when the sizes differ by less than 5 percent.

| Workload | RhizoDB | LadybugDB | SQLite | DuckDB | CozoDB | Neo4j | Memgraph | FalkorDB | Fastest |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| Point lookup by id | 0.541 | 0.976 | 0.296 | 45.7 | 3.90 | 54.7 | 31.4 | 15.7 | SQLite 1.8x |
| Filtered scan | 0.422 | 0.538 | 2.32 | 0.670 | 16.9 | 4.38 | 3.90 | 1.55 | RhizoDB 1.3x |
| `ORDER BY ... LIMIT` | 0.620 | 0.911 | 3.00 | 0.942 | 26.6 | 7.27 | 22.3 | 5.66 | similar |
| 1-hop neighbours | 0.452 | 0.769 | 0.081 | 9.84 | 1.41 | 14.7 | 6.28 | 3.67 | SQLite 5.6x |
| 1-hop with a relationship property | 0.578 | 0.740 | 0.371 | 10.5 | 1.21 | 12.9 | 6.97 | 4.18 | similar |
| Sum of a relationship property | 3.01 | 1.28 | 6.06 | 0.771 | 53.2 | 105 | 56.1 | 102 | DuckDB 1.7x |
| 2-hop distinct | 0.776 | 12.9 | 0.716 | 33.8 | 3.38 | 16.1 | 7.99 | 5.32 | similar |
| 3-hop distinct | 4.87 | 43.0 | 5.38 | 60.7 | 33.6 | 26.7 | 13.1 | 11.4 | RhizoDB 1.1x |
| Variable length `*1..3` | 5.35 | 33.8 | 9.08 | 126 | 33.1 | 26.9 | 14.1 | 9.96 | RhizoDB 1.7x |
| Group by city | 2.44 | 3.46 | 10.3 | 5.64 | 74.4 | 10.9 | 15.4 | 7.16 | similar |
| Substring scan of a text property | 22.5 | 28.6 | 76.7 | 58.8 | 374 | 208 | 109 | 59.9 | RhizoDB 1.3x |
| Keyword search | 53.6 | 362 | 187 | 788 | 5884 | 146 | 135 | 89.2 | RhizoDB 1.7x |
| Small commits | 67.9 | 204 | 80.3 | 578 | 34.1 | 428 | 11761 | 107 | CozoDB 2.0x |
| Bulk update | 13.2 | 13.0 | 41.1 | 8.59 | 51.8 | 55.0 | 90.8 | 8.18 | similar |
| Detach delete | 255 | 370 | 146 | 82.8 | 468 | 579 | 81.7 | 75.7 | similar |
| Bulk load | 668 | 922 | 310 | 628 | 2558 | 5593 | 3424 | 10831 | SQLite 2.0x |
| File size after bulk load | 12.9 MiB | 13.0 MiB | 16.4 MiB | 16.3 MiB | 25.5 MiB | n/a | n/a | n/a | similar |
<!-- engines-table:end -->

A row times a whole batch of operations, so compare engines within a row.
A server answers over a local socket, so its times include a round trip that an embedded engine does not pay.
An engine whose language has no form of a workload shows `unsupported`, and a server reports no file size.
